use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use pingora_reverse_proxy::route::{RouteData, RouteKey};
use pingora_reverse_proxy::route_table::{
    install_route_mutation_panic_hook_at_startup, MutationOperation, MutationSeal, RouteMatch,
    RouteRegistry, DETACHED_MUTATION_DIAGNOSTIC_CAPACITY, MUTATION_ADMISSION_SEALED_ERROR,
    MUTATION_PANIC_SEALED_ERROR,
};
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::redis::{RedisStore, RedisStoreConfig, DEFAULT_REDIS_ROUTE_KEY};
use pingora_reverse_proxy::store::{ActivityFloor, Store, StoreError};
use proptest::prelude::*;
use redis::AsyncCommands;
use serde_json::{json, Map};
use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Barrier, RwLock, Semaphore};

fn key(path: &str) -> RouteKey {
    RouteKey::parse(path).unwrap()
}

fn route(target: &str) -> RouteData {
    RouteData {
        target: target.to_owned(),
        last_activity: Utc.timestamp_opt(1, 0).unwrap(),
        extra: Map::from_iter([("owner".to_owned(), json!("jupyterhub"))]),
    }
}

fn memory_store() -> Arc<dyn Store> {
    Arc::new(MemoryStore::new())
}

async fn assert_store_contract(store: Arc<dyn Store>) {
    let route_key = key("//user/alice///");
    let add_started_at = Utc::now();
    let added = store
        .add(
            route_key.clone(),
            "http://127.0.0.1:8999/added".to_owned(),
            Map::from_iter([("owner".to_owned(), json!("added"))]),
            ActivityFloor::fixed(None),
        )
        .await
        .unwrap();
    assert!(added.last_activity >= add_started_at);
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&added),
        "add must return the exact atomically committed record"
    );

    let original = store
        .put_preserving_activity(
            route_key.clone(),
            route("http://127.0.0.1:9000/base"),
            ActivityFloor::fixed(None),
        )
        .await
        .unwrap();
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&original)
    );

    let activity_floor = Utc.timestamp_opt(7, 0).unwrap();
    let replacement = store
        .put_preserving_activity(
            route_key.clone(),
            route("http://127.0.0.1:9001/replaced"),
            ActivityFloor::fixed(Some(activity_floor)),
        )
        .await
        .unwrap();
    assert_eq!(replacement.last_activity, activity_floor);
    assert_eq!(store.snapshot().await.unwrap().len(), 1);
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&replacement)
    );

    let activity = Utc.timestamp_opt(42, 123).unwrap();
    store.update_activity(&route_key, activity).await.unwrap();
    let updated = store.snapshot().await.unwrap().remove(&route_key).unwrap();
    assert_eq!(updated.target, replacement.target);
    assert_eq!(updated.extra, replacement.extra);
    assert_eq!(updated.last_activity, activity);

    let missing = key("/missing");
    store
        .update_activity(&missing, Utc.timestamp_opt(99, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(store.delete(&missing).await.unwrap(), None);

    assert_eq!(store.delete(&route_key).await.unwrap(), Some(updated));
    assert!(store.snapshot().await.unwrap().is_empty());
}

#[tokio::test]
async fn memory_store_satisfies_backend_neutral_contract() {
    assert_store_contract(memory_store()).await;
}

fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL must name the disposable Redis")
}

fn redis_hash_key(test_name: &str) -> String {
    format!("{DEFAULT_REDIS_ROUTE_KEY}:test:{test_name}")
}

async fn redis_store(test_name: &str) -> RedisStore {
    RedisStore::connect(RedisStoreConfig::new(redis_url()).with_key(redis_hash_key(test_name)))
        .await
        .unwrap()
}

async fn clear_redis_hash(test_name: &str) {
    let client = redis::Client::open(redis_url()).unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let _: usize = connection.del(redis_hash_key(test_name)).await.unwrap();
}

async fn redis_client_ids(connection: &mut redis::aio::MultiplexedConnection) -> BTreeSet<u64> {
    let clients: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query_async(connection)
        .await
        .unwrap();
    clients
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("id="))
                .map(|id| id.parse().unwrap())
        })
        .collect()
}

async fn redis_exec_calls(connection: &mut redis::aio::MultiplexedConnection) -> u64 {
    let command_stats: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(connection)
        .await
        .unwrap();
    command_stats
        .lines()
        .find_map(|line| line.strip_prefix("cmdstat_exec:calls="))
        .and_then(|statistics| statistics.split(',').next())
        .map(|calls| calls.parse().unwrap())
        .unwrap_or_default()
}

struct SuppressMutationReplyProxy {
    url: String,
    suppressed: tokio::sync::oneshot::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl SuppressMutationReplyProxy {
    async fn start(marker: &str) -> Self {
        let backend = url::Url::parse(&redis_url()).unwrap();
        let backend_address = format!(
            "{}:{}",
            backend.host_str().unwrap(),
            backend.port_or_known_default().unwrap()
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let marker = marker.as_bytes().to_vec();
        let triggered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (suppressed_tx, suppressed) = tokio::sync::oneshot::channel();
        let suppressed_tx = Arc::new(std::sync::Mutex::new(Some(suppressed_tx)));
        let task = tokio::spawn(async move {
            loop {
                let (client, _) = listener.accept().await.unwrap();
                let server = TcpStream::connect(&backend_address).await.unwrap();
                let marker = marker.clone();
                let triggered = triggered.clone();
                let suppressed_tx = suppressed_tx.clone();
                tokio::spawn(async move {
                    let (mut client_read, mut client_write) = client.into_split();
                    let (mut server_read, mut server_write) = server.into_split();
                    let request_triggered = triggered.clone();
                    let request = tokio::spawn(async move {
                        let mut buffer = [0_u8; 16 * 1024];
                        loop {
                            let read = client_read.read(&mut buffer).await.unwrap();
                            if read == 0 {
                                break;
                            }
                            server_write.write_all(&buffer[..read]).await.unwrap();
                            if buffer[..read]
                                .windows(marker.len())
                                .any(|window| window == marker)
                            {
                                request_triggered.store(true, Ordering::SeqCst);
                            }
                        }
                    });
                    let response = tokio::spawn(async move {
                        let mut buffer = [0_u8; 16 * 1024];
                        loop {
                            let read = server_read.read(&mut buffer).await.unwrap();
                            if read == 0 {
                                break;
                            }
                            if triggered.swap(false, Ordering::SeqCst) {
                                if let Some(sender) = suppressed_tx.lock().unwrap().take() {
                                    let _ = sender.send(());
                                }
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                break;
                            }
                            client_write.write_all(&buffer[..read]).await.unwrap();
                        }
                    });
                    let _ = tokio::join!(request, response);
                });
            }
        });
        Self {
            url: format!("redis://{address}/"),
            suppressed,
            task,
        }
    }

    async fn wait_for_suppressed_reply(&mut self) {
        (&mut self.suppressed).await.unwrap();
    }
}

impl Drop for SuppressMutationReplyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fault_proxy_store(
    test_name: &str,
    marker: &str,
) -> (RedisStore, SuppressMutationReplyProxy) {
    let proxy = SuppressMutationReplyProxy::start(marker).await;
    let store = RedisStore::connect(
        RedisStoreConfig::new(proxy.url.clone())
            .with_key(redis_hash_key(test_name))
            .with_operation_timeout(Duration::from_millis(100)),
    )
    .await
    .unwrap();
    (store, proxy)
}

#[tokio::test]
#[serial(redis)]
async fn redis_store_satisfies_backend_neutral_contract() {
    const TEST_NAME: &str = "shared-contract";
    clear_redis_hash(TEST_NAME).await;
    assert_store_contract(Arc::new(redis_store(TEST_NAME).await)).await;
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_overwrite_delete_missing_and_metadata_round_trip() {
    const TEST_NAME: &str = "metadata";
    clear_redis_hash(TEST_NAME).await;
    let store = redis_store(TEST_NAME).await;
    let route_key = key("//user/alice///");
    let original = RouteData {
        target: "http://original.example/base".to_owned(),
        last_activity: Utc.timestamp_opt(123, 456_000_000).unwrap(),
        extra: Map::from_iter([
            ("owner".to_owned(), json!("jupyterhub")),
            (
                "nested".to_owned(),
                json!({"roles": ["user", "admin"], "enabled": true}),
            ),
            ("nullable".to_owned(), json!(null)),
        ]),
    };
    store.put(route_key.clone(), original).await.unwrap();

    let replacement = RouteData {
        target: "https://replacement.example/path?query=1".to_owned(),
        last_activity: Utc.timestamp_opt(987, 654_000_000).unwrap(),
        extra: Map::from_iter([
            ("owner".to_owned(), json!("replacement")),
            (
                "arbitrary".to_owned(),
                json!({"deep": {"array": [1, "two", false]}}),
            ),
        ]),
    };
    store
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();

    assert_eq!(store.snapshot().await.unwrap().len(), 1);
    assert_eq!(store.snapshot().await.unwrap()[&route_key], replacement);
    let client = redis::Client::open(redis_url()).unwrap();
    let mut raw_connection = client.get_multiplexed_async_connection().await.unwrap();
    let raw_records: Vec<(String, String)> = raw_connection
        .hgetall(redis_hash_key(TEST_NAME))
        .await
        .unwrap();
    assert_eq!(raw_records.len(), 1);
    assert_eq!(raw_records[0].0, route_key.as_str());
    let raw: serde_json::Value = serde_json::from_str(&raw_records[0].1).unwrap();
    assert_eq!(raw["target"], replacement.target);
    assert_eq!(raw["last_activity"], "1970-01-01T00:16:27.654000000Z");
    assert_eq!(raw["owner"], "replacement");
    assert_eq!(
        raw["arbitrary"],
        json!({"deep": {"array": [1, "two", false]}})
    );
    assert_eq!(raw.as_object().unwrap().len(), 4);
    assert_eq!(store.delete(&key("/missing")).await.unwrap(), None);
    assert_eq!(store.delete(&route_key).await.unwrap(), Some(replacement));
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_corrupt_snapshot_is_typed_redacted_and_never_published() {
    const TEST_NAME: &str = "corrupt-snapshot";
    clear_redis_hash(TEST_NAME).await;
    let hash_key = redis_hash_key(TEST_NAME);
    let client = redis::Client::open(redis_url()).unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let corrupt = r#"{"target":"http://secret.example","last_activity":false}"#;
    let _: usize = connection
        .hset(&hash_key, "/corrupt", corrupt)
        .await
        .unwrap();

    let store = Arc::new(redis_store(TEST_NAME).await);
    let error = match RouteRegistry::load(store).await {
        Ok(_) => panic!("corrupt Redis data must prevent registry publication"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        StoreError::CorruptData {
            operation: "snapshot",
            ..
        }
    ));
    let rendered = error.to_string();
    assert!(!rendered.contains(corrupt));
    assert!(!rendered.contains("secret.example"));
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_corrupt_watched_record_poisoned_connection_is_not_reused() {
    const TEST_NAME: &str = "corrupt-watched-record";
    clear_redis_hash(TEST_NAME).await;
    let client = redis::Client::open(redis_url()).unwrap();
    let mut administrator = client.get_multiplexed_async_connection().await.unwrap();
    let before = redis_client_ids(&mut administrator).await;
    let store = redis_store(TEST_NAME).await;
    let connected = redis_client_ids(&mut administrator).await;
    let store_id = *connected.difference(&before).next().unwrap();
    let route_key = key("/corrupt-watched");
    let _: usize = administrator
        .hset(
            redis_hash_key(TEST_NAME),
            route_key.as_str(),
            r#"{"target":false}"#,
        )
        .await
        .unwrap();

    let error = store
        .update_activity(&route_key, Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::CorruptData {
            operation: "update_activity",
            ..
        }
    ));
    tokio::task::yield_now().await;
    assert!(
        !redis_client_ids(&mut administrator)
            .await
            .contains(&store_id),
        "a connection that may still be WATCHing must be replaced"
    );

    let replacement = route("http://replacement-after-corruption.example");
    store
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();
    assert_eq!(store.snapshot().await.unwrap()[&route_key], replacement);
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_unavailable_startup_is_bounded_typed_and_redacts_credentials() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let secret = "redis-test-password-never-render";
    let url = format!("redis://default:{secret}@127.0.0.1:{port}/");
    let config = RedisStoreConfig::new(url);
    assert!(!format!("{config:?}").contains(secret));

    let error = tokio::time::timeout(Duration::from_secs(3), RedisStore::connect(config))
        .await
        .expect("startup must have a finite connection bound")
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect",
            ..
        }
    ));
    assert!(!error.to_string().contains(secret));
}

#[tokio::test]
#[serial(redis)]
async fn redis_disconnect_during_mutation_returns_typed_error_without_applying_write() {
    const TEST_NAME: &str = "disconnect";
    clear_redis_hash(TEST_NAME).await;
    let client = redis::Client::open(redis_url()).unwrap();
    let mut administrator = client.get_multiplexed_async_connection().await.unwrap();
    let before = redis_client_ids(&mut administrator).await;
    let store = redis_store(TEST_NAME).await;
    let after = redis_client_ids(&mut administrator).await;
    let store_ids: Vec<_> = after.difference(&before).copied().collect();
    assert_eq!(store_ids.len(), 1, "exactly one store client must connect");
    let killed: usize = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(store_ids[0])
        .query_async(&mut administrator)
        .await
        .unwrap();
    assert_eq!(killed, 1, "only the exact store connection is disconnected");

    let error = store
        .put(key("/not-applied"), route("http://not-applied.example"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "put",
            ..
        }
    ));

    let mut verification = client.get_multiplexed_async_connection().await.unwrap();
    let exists: bool = verification
        .hexists(redis_hash_key(TEST_NAME), "/not-applied")
        .await
        .unwrap();
    assert!(!exists);
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_reconciles_committed_mutations_when_their_replies_are_lost() {
    const TEST_NAME: &str = "lost-mutation-replies";
    clear_redis_hash(TEST_NAME).await;
    let route_key = key("/uncertain");

    let put_marker = "lost-put-reply.example";
    let (put_store, mut put_proxy) = fault_proxy_store(TEST_NAME, put_marker).await;
    let put_data = route(&format!("http://{put_marker}"));
    put_store
        .put(route_key.clone(), put_data.clone())
        .await
        .expect("a committed put with a lost reply must reconcile as success");
    put_proxy.wait_for_suppressed_reply().await;
    assert_eq!(put_store.snapshot().await.unwrap()[&route_key], put_data);
    drop(put_store);
    drop(put_proxy);

    let add_marker = "lost-add-reply.example";
    let (add_store, mut add_proxy) = fault_proxy_store(TEST_NAME, add_marker).await;
    let added = add_store
        .add(
            route_key.clone(),
            format!("http://{add_marker}"),
            Map::from_iter([("mutation".to_owned(), json!("add"))]),
            ActivityFloor::fixed(None),
        )
        .await
        .expect("a committed add with a lost EXEC reply must reconcile as success");
    add_proxy.wait_for_suppressed_reply().await;
    assert_eq!(add_store.snapshot().await.unwrap()[&route_key], added);
    drop(add_store);
    drop(add_proxy);

    let activity = Utc.timestamp_opt(7_654_321, 123_456_789).unwrap();
    let activity_marker = activity.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let (activity_store, mut activity_proxy) = fault_proxy_store(TEST_NAME, &activity_marker).await;
    activity_store
        .update_activity(&route_key, activity)
        .await
        .expect("a committed activity update with a lost EXEC reply must reconcile as success");
    activity_proxy.wait_for_suppressed_reply().await;
    assert_eq!(
        activity_store.snapshot().await.unwrap()[&route_key].last_activity,
        activity
    );
    drop(activity_store);
    drop(activity_proxy);

    let delete_marker = "HDEL";
    let (delete_store, mut delete_proxy) = fault_proxy_store(TEST_NAME, delete_marker).await;
    let deleted = delete_store
        .delete(&route_key)
        .await
        .expect("a committed delete with a lost reply must reconcile as success");
    delete_proxy.wait_for_suppressed_reply().await;
    assert!(deleted.is_some());
    assert!(delete_store.snapshot().await.unwrap().is_empty());
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial(redis)]
async fn redis_two_stores_retry_forced_exec_contention() {
    const TEST_NAME: &str = "two-store-contention";
    const ROUNDS: u64 = 12;
    clear_redis_hash(TEST_NAME).await;
    let first = Arc::new(redis_store(TEST_NAME).await);
    let second = Arc::new(redis_store(TEST_NAME).await);
    let client = redis::Client::open(redis_url()).unwrap();
    let mut administrator = client.get_multiplexed_async_connection().await.unwrap();
    let exec_calls_before = redis_exec_calls(&mut administrator).await;
    let large_metadata = "x".repeat(512 * 1024);

    for round in 0..ROUNDS {
        let barrier = Arc::new(Barrier::new(3));
        let first_task = {
            let store = first.clone();
            let barrier = barrier.clone();
            let large_metadata = large_metadata.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store
                    .add(
                        key("/contended"),
                        format!("http://first-{round}.example"),
                        Map::from_iter([("padding".to_owned(), json!(large_metadata))]),
                        ActivityFloor::fixed(None),
                    )
                    .await
                    .unwrap();
            })
        };
        let second_task = {
            let store = second.clone();
            let barrier = barrier.clone();
            let large_metadata = large_metadata.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store
                    .add(
                        key("/contended"),
                        format!("http://second-{round}.example"),
                        Map::from_iter([("padding".to_owned(), json!(large_metadata))]),
                        ActivityFloor::fixed(None),
                    )
                    .await
                    .unwrap();
            })
        };
        barrier.wait().await;
        first_task.await.unwrap();
        second_task.await.unwrap();
    }

    let exec_calls = redis_exec_calls(&mut administrator).await - exec_calls_before;
    assert!(
        exec_calls > ROUNDS * 2,
        "at least one watched transaction must abort and retry: {exec_calls} EXEC calls"
    );
    assert_eq!(first.snapshot().await.unwrap().len(), 1);
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_restart_persistence_recovers_complete_routes_without_clearing() {
    const TEST_NAME: &str = "restart-persistence";
    let Ok(phase) = std::env::var("REDIS_RESTART_PHASE") else {
        return;
    };
    let store = Arc::new(redis_store(TEST_NAME).await);
    let route_key = key("//persistent/user///");
    let expected = RouteData {
        target: "http://persistent.example/base".to_owned(),
        last_activity: Utc.timestamp_opt(1_234_567, 890_000_000).unwrap(),
        extra: Map::from_iter([
            ("owner".to_owned(), json!("restart-test")),
            (
                "metadata".to_owned(),
                json!({"survives": ["process", "restart"]}),
            ),
        ]),
    };

    match phase.as_str() {
        "writer" => {
            clear_redis_hash(TEST_NAME).await;
            let registry = RouteRegistry::load(store).await.unwrap();
            assert!(registry.get(&route_key).is_none());
            registry.put(route_key, expected).await.unwrap();
        }
        "reader" => {
            let registry = RouteRegistry::load(store).await.unwrap();
            assert_eq!(registry.get(&route_key), Some(expected));
            clear_redis_hash(TEST_NAME).await;
        }
        phase => panic!("unknown REDIS_RESTART_PHASE {phase:?}"),
    }
}

#[derive(Clone, Debug)]
enum Operation {
    Put(u8, u8),
    Activity(u8, i64),
    Delete(u8),
}

fn operation_strategy() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (0u8..8, any::<u8>()).prop_map(|(key, route)| Operation::Put(key, route)),
        (0u8..8, 0i64..10_000).prop_map(|(key, at)| Operation::Activity(key, at)),
        (0u8..8).prop_map(Operation::Delete),
    ]
}

proptest! {
    #[test]
    fn memory_store_matches_reference_state_machine(
        operations in prop::collection::vec(operation_strategy(), 0..64),
    ) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let store = memory_store();
            let mut reference = BTreeMap::new();

            for operation in operations {
                match operation {
                    Operation::Put(id, route_id) => {
                        let route_key = key(&format!("/route/{id}"));
                        let route_data = route(&format!("http://127.0.0.1:{}/", 10_000 + u16::from(route_id)));
                        let activity_floor = reference
                            .get(&route_key)
                            .map(|route: &RouteData| route.last_activity);
                        let route_data = store
                            .put_preserving_activity(
                                route_key.clone(),
                                route_data,
                                ActivityFloor::fixed(activity_floor),
                            )
                            .await
                            .unwrap();
                        reference.insert(route_key, route_data);
                    }
                    Operation::Activity(id, seconds) => {
                        let route_key = key(&format!("/route/{id}"));
                        let at = Utc.timestamp_opt(seconds, 0).unwrap();
                        store.update_activity(&route_key, at).await.unwrap();
                        if let Some(route_data) = reference.get_mut(&route_key) {
                            route_data.last_activity = at;
                        }
                    }
                    Operation::Delete(id) => {
                        let route_key = key(&format!("/route/{id}"));
                        let actual = store.delete(&route_key).await.unwrap();
                        prop_assert_eq!(actual, reference.remove(&route_key));
                    }
                }

                prop_assert_eq!(store.snapshot().await.unwrap(), reference.clone());
            }

            Ok(())
        })?;
    }
}

struct FailingStore {
    routes: BTreeMap<RouteKey, RouteData>,
}

impl FailingStore {
    fn on_put() -> Self {
        Self::with_routes(BTreeMap::new())
    }

    fn with_routes(routes: BTreeMap<RouteKey, RouteData>) -> Self {
        Self { routes }
    }
}

#[async_trait]
impl Store for FailingStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("injected add failure"))
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        Err(StoreError::message("injected put failure"))
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("injected put failure"))
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        Err(StoreError::message("injected activity failure"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("injected delete failure"))
    }
}

#[tokio::test]
async fn failed_persistence_never_publishes_route() {
    let store = Arc::new(FailingStore::on_put());
    let registry = RouteRegistry::load(store).await.unwrap();
    let result = registry
        .put(key("/user/a"), route("http://127.0.0.1:9000"))
        .await;
    assert!(result.is_err());
    assert!(registry.resolve("/user/a/tree").is_none());
}

#[tokio::test]
async fn failed_atomic_put_changes_no_backend_state_and_fresh_reload_matches() {
    let route_key = key("/user/a");
    let original = route("http://original.example");
    let store = Arc::new(FailingStore::with_routes(BTreeMap::from([(
        route_key.clone(),
        original.clone(),
    )])));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let observed = Utc.timestamp_opt(999, 0).unwrap();
    assert!(registry.observe_activity(&route_key, observed));

    assert!(registry
        .put(route_key.clone(), route("http://replacement.example"))
        .await
        .is_err());
    assert_eq!(store.snapshot().await.unwrap()[&route_key], original);
    let reloaded = RouteRegistry::load(store).await.unwrap();
    assert_eq!(reloaded.get(&route_key), Some(original));
}

struct GatedAtomicAddStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    entered: Semaphore,
    release: Semaphore,
    fail: bool,
}

impl GatedAtomicAddStore {
    fn new(routes: BTreeMap<RouteKey, RouteData>, fail: bool) -> Self {
        Self {
            routes: RwLock::new(routes),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            fail,
        }
    }
}

#[async_trait]
impl Store for GatedAtomicAddStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        if self.fail {
            return Err(StoreError::message("injected atomic add failure"));
        }

        let mut routes = self.routes.write().await;
        let mut data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        if let Some(route) = self.routes.write().await.get_mut(key) {
            route.last_activity = at;
        }
        Ok(())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Ok(self.routes.write().await.remove(key))
    }
}

#[tokio::test]
async fn atomic_add_failure_leaves_backend_and_registry_unchanged() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let store = Arc::new(GatedAtomicAddStore::new(
        BTreeMap::from([(route_key.clone(), original.clone())]),
        true,
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let adding = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(
                    route_key,
                    "http://replacement.example".to_owned(),
                    Map::new(),
                )
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    store.release.add_permits(1);
    assert!(adding.await.unwrap().is_err());

    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&original)
    );
    assert_eq!(registry.get(&route_key), Some(original));
}

#[tokio::test]
async fn cancelling_add_before_commit_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let store = Arc::new(GatedAtomicAddStore::new(
        BTreeMap::from([(route_key.clone(), original.clone())]),
        false,
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let adding = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(
                    route_key,
                    "http://replacement.example".to_owned(),
                    Map::new(),
                )
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    adding.abort();
    assert!(adding.await.unwrap_err().is_cancelled());
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&original)
    );
    assert_eq!(registry.get(&route_key), Some(original));

    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    let committed = store.snapshot().await.unwrap().remove(&route_key).unwrap();
    assert_eq!(committed.target, "http://replacement.example");
    assert_eq!(registry.get(&route_key), Some(committed));
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GatedMutation {
    Put,
    UpdateActivity,
    Delete,
}

struct GatedMutationStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    gated: GatedMutation,
    entered: Semaphore,
    release: Semaphore,
}

impl GatedMutationStore {
    fn new(gated: GatedMutation, routes: BTreeMap<RouteKey, RouteData>) -> Self {
        Self {
            routes: RwLock::new(routes),
            gated,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    async fn gate(&self, mutation: GatedMutation) {
        if self.gated == mutation {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
    }
}

#[async_trait]
impl Store for GatedMutationStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.gate(GatedMutation::Put).await;
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.gate(GatedMutation::Put).await;
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        self.gate(GatedMutation::UpdateActivity).await;
        if let Some(route) = self.routes.write().await.get_mut(key) {
            route.last_activity = at;
        }
        Ok(())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        self.gate(GatedMutation::Delete).await;
        Ok(self.routes.write().await.remove(key))
    }
}

#[tokio::test]
async fn cancelling_put_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let replacement = route("http://replacement.example");
    let store = Arc::new(GatedMutationStore::new(GatedMutation::Put, BTreeMap::new()));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let caller = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        let replacement = replacement.clone();
        tokio::spawn(async move { registry.put(route_key, replacement).await })
    };

    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&replacement)
    );
    assert_eq!(registry.get(&route_key), Some(replacement));
}

#[tokio::test]
async fn put_samples_activity_after_method_entry_at_atomic_commit_and_survives_reload() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let observed = Utc.timestamp_opt(999, 0).unwrap();
    let store = Arc::new(GatedMutationStore::new(
        GatedMutation::Put,
        BTreeMap::from([(route_key.clone(), original)]),
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let putting = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .put(route_key, route("http://replacement.example"))
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    assert!(registry.observe_activity(&route_key, observed));
    store.release.add_permits(1);
    putting.await.unwrap().unwrap();

    let committed = store.snapshot().await.unwrap()[&route_key].clone();
    assert_eq!(committed.target, "http://replacement.example");
    assert_eq!(committed.last_activity, observed);
    assert_eq!(registry.get(&route_key), Some(committed.clone()));
    let reloaded = RouteRegistry::load(store).await.unwrap();
    assert_eq!(reloaded.get(&route_key), Some(committed));
}

#[tokio::test]
async fn cancelling_activity_update_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let activity = Utc.timestamp_opt(999, 0).unwrap();
    let store = Arc::new(GatedMutationStore::new(
        GatedMutation::UpdateActivity,
        BTreeMap::from([(route_key.clone(), original)]),
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    assert!(registry.observe_activity(&route_key, activity));
    let caller = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .persist_observed_activity(&route_key, activity)
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    assert_eq!(
        store.snapshot().await.unwrap()[&route_key].last_activity,
        activity
    );
    assert_eq!(registry.get(&route_key).unwrap().last_activity, activity);
}

#[tokio::test]
async fn cancelling_delete_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let store = Arc::new(GatedMutationStore::new(
        GatedMutation::Delete,
        BTreeMap::from([(route_key.clone(), route("http://original.example"))]),
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let caller = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move { registry.delete(&route_key).await })
    };

    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    assert!(!store.snapshot().await.unwrap().contains_key(&route_key));
    assert!(registry.get(&route_key).is_none());
}

struct TimingOutPutStore;

#[async_trait]
impl Store for TimingOutPutStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(BTreeMap::new())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("unused add"))
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
            .await
            .map_err(|_| StoreError::message("backend timeout"))
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
            .await
            .map_err(|_| StoreError::message("backend timeout"))?;
        unreachable!()
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        Err(StoreError::message("unused activity update"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("unused delete"))
    }
}

#[tokio::test]
async fn backend_timeout_finishes_the_mutation_task_and_releases_the_registry() {
    let registry = RouteRegistry::load(Arc::new(TimingOutPutStore))
        .await
        .unwrap();
    let registry_lifetime = Arc::downgrade(&registry);

    let error = registry
        .put(key("/service"), route("http://timeout.example"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "backend timeout");
    drop(registry);

    assert!(
        registry_lifetime.upgrade().is_none(),
        "a completed backend timeout must not leave a detached task retaining the registry"
    );
}

#[tokio::test]
async fn slow_atomic_add_stamps_after_delay_and_publishes_exact_committed_data() {
    let route_key = key("/service");
    let store = Arc::new(GatedAtomicAddStore::new(BTreeMap::new(), false));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let adding = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(route_key, "http://committed.example".to_owned(), Map::new())
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    let delay_finished_at = Utc::now();
    store.release.add_permits(1);
    adding.await.unwrap().unwrap();

    let committed = store.snapshot().await.unwrap().remove(&route_key).unwrap();
    assert!(committed.last_activity >= delay_finished_at);
    assert_eq!(registry.get(&route_key), Some(committed));
}

fn registry_views(
    registry: &RouteRegistry,
    route_key: &RouteKey,
    request_path: &str,
) -> (
    Option<RouteData>,
    BTreeMap<RouteKey, RouteData>,
    Option<RouteMatch>,
) {
    (
        registry.get(route_key),
        registry.all(),
        registry.resolve(request_path),
    )
}

#[tokio::test]
async fn failed_activity_update_leaves_every_registry_view_unchanged() {
    let route_key = key("/service");
    let route_data = route("http://127.0.0.1:9000/original");
    let store = Arc::new(FailingStore::with_routes(BTreeMap::from([(
        route_key.clone(),
        route_data,
    )])));
    let registry = RouteRegistry::load(store).await.unwrap();
    let before = registry_views(&registry, &route_key, "/service/request");

    let result = registry
        .update_activity(&route_key, Utc.timestamp_opt(999, 0).unwrap())
        .await;

    assert!(result.is_err());
    assert_eq!(
        registry_views(&registry, &route_key, "/service/request"),
        before
    );
}

#[tokio::test]
async fn failed_delete_leaves_every_registry_view_unchanged() {
    let route_key = key("/service");
    let route_data = route("http://127.0.0.1:9000/original");
    let store = Arc::new(FailingStore::with_routes(BTreeMap::from([(
        route_key.clone(),
        route_data,
    )])));
    let registry = RouteRegistry::load(store).await.unwrap();
    let before = registry_views(&registry, &route_key, "/service/request");

    let result = registry.delete(&route_key).await;

    assert!(result.is_err());
    assert_eq!(
        registry_views(&registry, &route_key, "/service/request"),
        before
    );
}

#[tokio::test]
async fn registry_loads_and_exposes_complete_store_snapshot() {
    let store = memory_store();
    let route_key = key("/user/a");
    let route_data = route("http://127.0.0.1:9000");
    store
        .put(route_key.clone(), route_data.clone())
        .await
        .unwrap();

    let registry = RouteRegistry::load(store).await.unwrap();

    assert_eq!(registry.get(&route_key), Some(route_data.clone()));
    assert_eq!(
        registry.all(),
        BTreeMap::from([(route_key.clone(), route_data)])
    );
    assert_eq!(registry.resolve("/user/a/tree").unwrap().key, route_key);
}

#[tokio::test]
async fn registry_overwrite_and_activity_update_publish_complete_records() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    let route_key = key("/service");
    registry
        .put(route_key.clone(), route("http://127.0.0.1:9000/original"))
        .await
        .unwrap();
    let replacement = route("http://127.0.0.1:9001/replacement");
    registry
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();

    let activity = Utc.timestamp_opt(500, 0).unwrap();
    registry
        .update_activity(&route_key, activity)
        .await
        .unwrap();

    let actual = registry.get(&route_key).unwrap();
    assert_eq!(actual.target, replacement.target);
    assert_eq!(actual.extra, replacement.extra);
    assert_eq!(actual.last_activity, activity);
    assert_eq!(registry.all().len(), 1);
}

#[tokio::test]
async fn aliasing_route_keys_follow_successful_mutation_order_in_both_permutations() {
    for (first, second) in [("/service", "//service"), ("//service", "/service")] {
        let registry = RouteRegistry::load(memory_store()).await.unwrap();
        registry
            .put(key(first), route(&format!("http://first.example{first}")))
            .await
            .unwrap();
        registry
            .put(
                key(second),
                route(&format!("http://second.example{second}")),
            )
            .await
            .unwrap();

        let matched = registry.resolve("/service/request").unwrap();
        assert_eq!(
            matched.key,
            key(second),
            "mutation order {first:?}, {second:?}"
        );
    }
}

#[tokio::test]
async fn overwriting_an_alias_moves_it_to_the_end_of_matcher_order() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    registry
        .put(key("/service"), route("http://single.example/old"))
        .await
        .unwrap();
    registry
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();
    registry
        .put(key("/service"), route("http://single.example/new"))
        .await
        .unwrap();

    let matched = registry.resolve("/service/request").unwrap();
    assert_eq!(matched.key, key("/service"));
    assert_eq!(matched.data.target, "http://single.example/new");
}

#[tokio::test]
async fn deleting_the_winning_alias_restores_the_surviving_alias() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    registry
        .put(key("/service"), route("http://single.example"))
        .await
        .unwrap();
    registry
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();

    registry.delete(&key("//service")).await.unwrap();

    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        key("/service")
    );
}

#[tokio::test]
async fn activity_updates_do_not_change_alias_matcher_order() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    registry
        .put(key("/service"), route("http://single.example"))
        .await
        .unwrap();
    registry
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();

    registry
        .update_activity(&key("/service"), Utc.timestamp_opt(500, 0).unwrap())
        .await
        .unwrap();

    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        key("//service")
    );
}

#[tokio::test]
async fn initial_store_load_uses_deterministic_key_order_for_aliases() {
    let store = memory_store();
    store
        .put(key("/service"), route("http://single.example"))
        .await
        .unwrap();
    store
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();

    let registry = RouteRegistry::load(store).await.unwrap();

    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        key("/service"),
        "initial load uses the snapshot's ascending RouteKey order"
    );
}

#[tokio::test]
async fn registry_delete_missing_is_a_noop_and_delete_returns_prior_route() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    let route_key = key("/service");
    let route_data = route("http://127.0.0.1:9000");
    registry
        .put(route_key.clone(), route_data.clone())
        .await
        .unwrap();

    assert_eq!(registry.delete(&key("/missing")).await.unwrap(), None);
    assert_eq!(registry.delete(&route_key).await.unwrap(), Some(route_data));
    assert!(registry.get(&route_key).is_none());
    assert!(registry.resolve("/service/tree").is_none());
}

#[derive(Clone, Copy)]
enum GatedManagementOperation {
    Add,
    Put,
    Delete,
}

struct GatedManagementStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    operation: GatedManagementOperation,
    entered: Semaphore,
    release: Semaphore,
}

impl GatedManagementStore {
    fn new(routes: BTreeMap<RouteKey, RouteData>, operation: GatedManagementOperation) -> Self {
        Self {
            routes: RwLock::new(routes),
            operation,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    async fn gate(&self) {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
    }
}

#[async_trait]
impl Store for GatedManagementStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        assert!(matches!(self.operation, GatedManagementOperation::Add));
        self.gate().await;
        let mut data = RouteData {
            target,
            last_activity: Utc.timestamp_opt(20, 0).unwrap(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        assert!(matches!(self.operation, GatedManagementOperation::Put));
        self.gate().await;
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        assert!(matches!(self.operation, GatedManagementOperation::Put));
        self.gate().await;
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        if let Some(route) = self.routes.write().await.get_mut(key) {
            route.last_activity = at;
        }
        Ok(())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        assert!(matches!(self.operation, GatedManagementOperation::Delete));
        self.gate().await;
        Ok(self.routes.write().await.remove(key))
    }
}

async fn assert_blocked_management_preserves_other_route_activity(
    operation: GatedManagementOperation,
) {
    let activity_key = key("/active");
    let mutation_key = key("/mutated");
    let initial = BTreeMap::from([
        (activity_key.clone(), route("http://active.example")),
        (mutation_key.clone(), route("http://old.example")),
    ]);
    let store = Arc::new(GatedManagementStore::new(initial, operation));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    let mutation = {
        let registry = Arc::clone(&registry);
        let mutation_key = mutation_key.clone();
        tokio::spawn(async move {
            match operation {
                GatedManagementOperation::Add => registry
                    .add(mutation_key, "http://added.example".to_owned(), Map::new())
                    .await
                    .map(|_| ()),
                GatedManagementOperation::Put => {
                    registry
                        .put(mutation_key, route("http://put.example"))
                        .await
                }
                GatedManagementOperation::Delete => {
                    registry.delete(&mutation_key).await.map(|_| ())
                }
            }
        })
    };
    store.entered.acquire().await.unwrap().forget();

    let observed_at = Utc.timestamp_opt(30, 0).unwrap();
    assert!(registry.observe_activity(&activity_key, observed_at));
    assert_eq!(
        registry.get(&activity_key).unwrap().last_activity,
        observed_at
    );
    assert!(registry.observe_activity(&mutation_key, observed_at));

    store.release.add_permits(1);
    mutation.await.unwrap().unwrap();
    assert_eq!(
        registry.get(&activity_key).unwrap().last_activity,
        observed_at,
        "a persistence-first management publication must merge with the latest activity snapshot"
    );
    let persisted = store.snapshot().await.unwrap();
    let reloaded = RouteRegistry::load(store.clone()).await.unwrap();
    assert_eq!(reloaded.all(), persisted);
    if matches!(operation, GatedManagementOperation::Delete) {
        assert!(!persisted.contains_key(&mutation_key));
    } else {
        assert_eq!(persisted[&mutation_key].last_activity, observed_at);
        assert_eq!(
            registry.get(&mutation_key),
            Some(persisted[&mutation_key].clone())
        );
    }
}

#[tokio::test]
async fn blocked_add_merges_activity_observed_on_another_route() {
    assert_blocked_management_preserves_other_route_activity(GatedManagementOperation::Add).await;
}

#[tokio::test]
async fn blocked_put_merges_activity_observed_on_another_route() {
    assert_blocked_management_preserves_other_route_activity(GatedManagementOperation::Put).await;
}

#[tokio::test]
async fn blocked_delete_merges_activity_observed_on_another_route() {
    assert_blocked_management_preserves_other_route_activity(GatedManagementOperation::Delete)
        .await;
}

struct GatedPutStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    gates: BTreeMap<RouteKey, Arc<Semaphore>>,
    entered: Semaphore,
    entries: std::sync::Mutex<Vec<RouteKey>>,
}

impl GatedPutStore {
    fn new(route_keys: impl IntoIterator<Item = RouteKey>) -> Self {
        Self {
            routes: RwLock::new(BTreeMap::new()),
            gates: route_keys
                .into_iter()
                .map(|route_key| (route_key, Arc::new(Semaphore::new(0))))
                .collect(),
            entered: Semaphore::new(0),
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn release(&self, route_key: &RouteKey) {
        self.gates[route_key].add_permits(1);
    }

    fn entries(&self) -> Vec<RouteKey> {
        self.entries.lock().unwrap().clone()
    }
}

#[async_trait]
impl Store for GatedPutStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.entries.lock().unwrap().push(key.clone());
        self.entered.add_permits(1);
        self.gates[&key].acquire().await.unwrap().forget();
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.entries.lock().unwrap().push(key.clone());
        self.entered.add_permits(1);
        self.gates[&key].acquire().await.unwrap().forget();
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }
}

#[tokio::test]
async fn concurrent_alias_writers_are_serialized_in_successful_mutation_order() {
    let first_key = key("/service");
    let second_key = key("//service");
    let store = Arc::new(GatedPutStore::new([first_key.clone(), second_key.clone()]));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let first_data = route("http://127.0.0.1:9001/first");
    let second_data = route("http://127.0.0.1:9002/second");

    let first = {
        let registry = Arc::clone(&registry);
        let first_key = first_key.clone();
        let first_data = first_data.clone();
        tokio::spawn(async move { registry.put(first_key, first_data).await })
    };
    store.entered.acquire().await.unwrap().forget();
    assert_eq!(store.entries(), vec![first_key.clone()]);
    let second = {
        let registry = Arc::clone(&registry);
        let second_key = second_key.clone();
        let second_data = second_data.clone();
        tokio::spawn(async move { registry.put(second_key, second_data).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(
        store.entries(),
        vec![first_key.clone()],
        "the second writer must not clone a stale snapshot or enter persistence"
    );

    store.release(&first_key);
    first.await.unwrap().unwrap();
    store.entered.acquire().await.unwrap().forget();
    assert_eq!(store.entries(), vec![first_key.clone(), second_key.clone()]);
    store.release(&second_key);
    second.await.unwrap().unwrap();

    let expected = BTreeMap::from([
        (first_key.clone(), first_data.clone()),
        (second_key.clone(), second_data.clone()),
    ]);
    assert_eq!(registry.all(), expected);
    assert_eq!(registry.get(&first_key), Some(first_data));
    assert_eq!(registry.get(&second_key), Some(second_data));
    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        second_key
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_observe_only_complete_snapshots() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    let route_key = key("/service");
    let old = route("http://old.example/generation-0");
    registry.put(route_key.clone(), old.clone()).await.unwrap();
    let start = Arc::new(Barrier::new(5));
    let old_observed = Arc::new(Barrier::new(5));
    let new_published = Arc::new(Barrier::new(5));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let registry = Arc::clone(&registry);
            let start = Arc::clone(&start);
            let old_observed = Arc::clone(&old_observed);
            let new_published = Arc::clone(&new_published);
            tokio::spawn(async move {
                start.wait().await;
                tokio::task::yield_now().await;
                let before = (*registry.resolve("/service/request").unwrap().data).clone();
                old_observed.wait().await;
                new_published.wait().await;
                tokio::task::yield_now().await;
                let after = (*registry.resolve("/service/request").unwrap().data).clone();
                (before, after)
            })
        })
        .collect();

    start.wait().await;
    old_observed.wait().await;
    tokio::task::yield_now().await;
    let mut replacement = route("http://new.example/generation-1");
    replacement.last_activity = Utc.timestamp_opt(2, 0).unwrap();
    registry.put(route_key, replacement.clone()).await.unwrap();
    tokio::task::yield_now().await;
    new_published.wait().await;

    for reader in readers {
        let (before, after) = reader.await.unwrap();
        assert_eq!(
            before, old,
            "reader did not observe the complete old snapshot"
        );
        assert_eq!(
            after, replacement,
            "reader did not observe the complete new snapshot"
        );
    }

    assert!(registry.resolve("\0/untrusted/runtime/path").is_none());
}

#[derive(Clone, Copy)]
enum SupervisedPutOutcome {
    Success,
    Error,
    Panic,
    PanicWithPanickingPayload,
}

struct PanickingPayloadDrop;

impl Drop for PanickingPayloadDrop {
    fn drop(&mut self) {
        panic!("PANICKING_PAYLOAD_DROP_SENTINEL_f6269624");
    }
}

struct SupervisedPutStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    entered: Semaphore,
    release: Semaphore,
    outcome: SupervisedPutOutcome,
}

impl SupervisedPutStore {
    fn new(outcome: SupervisedPutOutcome) -> Self {
        Self {
            routes: RwLock::new(BTreeMap::new()),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            outcome,
        }
    }
}

#[async_trait]
impl Store for SupervisedPutStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("unused add"))
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        match self.outcome {
            SupervisedPutOutcome::Success => {
                self.routes.write().await.insert(key, data);
                Ok(())
            }
            SupervisedPutOutcome::Error => Err(StoreError::message("detached backend failure")),
            SupervisedPutOutcome::Panic => {
                panic!("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e")
            }
            SupervisedPutOutcome::PanicWithPanickingPayload => {
                std::panic::panic_any(PanickingPayloadDrop)
            }
        }
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        match self.outcome {
            SupervisedPutOutcome::Success => {
                let mut routes = self.routes.write().await;
                if let Some(floor) = activity_floor.current() {
                    data.last_activity = data.last_activity.max(floor);
                }
                routes.insert(key, data.clone());
                Ok(data)
            }
            SupervisedPutOutcome::Error => Err(StoreError::message("detached backend failure")),
            SupervisedPutOutcome::Panic => {
                panic!("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e")
            }
            SupervisedPutOutcome::PanicWithPanickingPayload => {
                std::panic::panic_any(PanickingPayloadDrop)
            }
        }
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        Err(StoreError::message("unused activity update"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("unused delete"))
    }
}

#[test]
fn application_owned_panic_hook_delegates_unrelated_panics_and_redacts_mutations() {
    const CHILD_ENV: &str = "ROUTE_MUTATION_PANIC_HOOK_CHILD";
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "panic_hook_subprocess_child_receives_fixed_mutation_errors",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();

    assert!(output.status.success(), "child test failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("PREVIOUS_PANIC_HOOK_DELEGATED").count(), 1);
    assert!(stderr.contains("route mutation panic redacted at "));
    assert!(stderr.contains("tests/store_contract.rs:"));
    assert!(!stderr.contains("PREVIOUS_HOOK_SAW_MARKED_PANIC"));
    assert!(!stderr.contains("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e"));
    assert!(!stderr.contains("PANICKING_PAYLOAD_DROP_SENTINEL_f6269624"));
    assert!(!stderr.contains("UNRELATED_PANIC_SENTINEL_04bb8c85"));
}

#[test]
fn panic_hook_subprocess_child_receives_fixed_mutation_errors() {
    if std::env::var_os("ROUTE_MUTATION_PANIC_HOOK_CHILD").is_none() {
        return;
    }

    std::panic::set_hook(Box::new(|panic_info| {
        use std::io::Write as _;

        let unrelated = panic_info
            .payload()
            .downcast_ref::<&str>()
            .is_some_and(|payload| *payload == "UNRELATED_PANIC_SENTINEL_04bb8c85");
        let message = if unrelated {
            "PREVIOUS_PANIC_HOOK_DELEGATED"
        } else {
            "PREVIOUS_HOOK_SAW_MARKED_PANIC"
        };
        let _ = writeln!(std::io::stderr().lock(), "{message}");
    }));
    install_route_mutation_panic_hook_at_startup();

    let _ = std::panic::catch_unwind(|| panic!("UNRELATED_PANIC_SENTINEL_04bb8c85"));

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let live_store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
        let live_registry = RouteRegistry::load(live_store.clone()).await.unwrap();
        let live = tokio::spawn({
            let registry = Arc::clone(&live_registry);
            async move {
                registry
                    .put(key("/hook-live"), route("http://hook-live.example"))
                    .await
            }
        });
        live_store.entered.acquire().await.unwrap().forget();
        live_store.release.add_permits(1);
        assert_eq!(
            live.await.unwrap().unwrap_err().to_string(),
            "route put mutation task panicked"
        );
        let live_drain = live_registry.drain_mutations(Duration::from_secs(1)).await;
        assert!(!live_drain.timed_out);
        assert_eq!(live_drain.active_mutations, 0);
        assert!(live_drain.detached_panics.is_empty());

        let detached_store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
        let detached_registry = RouteRegistry::load(detached_store.clone()).await.unwrap();
        start_and_cancel_supervised_put(
            &detached_registry,
            &detached_store,
            key("/hook-detached"),
            route("http://hook-detached.example"),
        )
        .await;
        detached_store.release.add_permits(1);
        let drained = detached_registry
            .drain_mutations(Duration::from_secs(1))
            .await;
        assert!(!drained.timed_out);
        assert_eq!(drained.detached_panics.len(), 1);
        assert_eq!(
            drained.detached_panics[0].message,
            "route put mutation task panicked"
        );

        let drop_store = Arc::new(SupervisedPutStore::new(
            SupervisedPutOutcome::PanicWithPanickingPayload,
        ));
        let drop_registry = RouteRegistry::load(drop_store.clone()).await.unwrap();
        let drop_panic = tokio::spawn({
            let registry = Arc::clone(&drop_registry);
            async move {
                registry
                    .put(key("/hook-drop"), route("http://hook-drop.example"))
                    .await
            }
        });
        drop_store.entered.acquire().await.unwrap().forget();
        drop_store.release.add_permits(1);
        assert_eq!(
            drop_panic.await.unwrap().unwrap_err().to_string(),
            "route put mutation task panicked"
        );
    });
}

struct SnapshotFailureStore;

#[async_trait]
impl Store for SnapshotFailureStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Err(StoreError::message("injected snapshot failure"))
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!()
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        unreachable!()
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!()
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        unreachable!()
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        unreachable!()
    }
}

#[test]
fn registry_load_failure_does_not_install_or_replace_the_process_panic_hook() {
    const CHILD_ENV: &str = "ROUTE_LOAD_FAILURE_PANIC_HOOK_CHILD";
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "load_failure_subprocess_child_keeps_the_application_panic_hook",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();

    assert!(output.status.success(), "child test failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("LOAD_FAILURE_POST_HOOK_DELEGATED").count(),
        1
    );
    assert!(stderr.contains("route mutation panic redacted at "));
    assert!(!stderr.contains("LOAD_FAILURE_HOOK_SAW_MARKED_PANIC"));
    assert!(!stderr.contains("LOAD_FAILURE_UNRELATED_SENTINEL_109cbc43"));
    assert!(!stderr.contains("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e"));
}

#[test]
fn load_failure_subprocess_child_keeps_the_application_panic_hook() {
    if std::env::var_os("ROUTE_LOAD_FAILURE_PANIC_HOOK_CHILD").is_none() {
        return;
    }

    std::panic::set_hook(Box::new(|_| {}));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let Err(error) = runtime.block_on(RouteRegistry::load(Arc::new(SnapshotFailureStore))) else {
        panic!("snapshot failure must reject registry load");
    };
    assert_eq!(error.to_string(), "injected snapshot failure");

    drop(std::panic::take_hook());
    std::panic::set_hook(Box::new(|panic_info| {
        use std::io::Write as _;

        let unrelated = panic_info
            .payload()
            .downcast_ref::<&str>()
            .is_some_and(|payload| *payload == "LOAD_FAILURE_UNRELATED_SENTINEL_109cbc43");
        let message = if unrelated {
            "LOAD_FAILURE_POST_HOOK_DELEGATED"
        } else {
            "LOAD_FAILURE_HOOK_SAW_MARKED_PANIC"
        };
        let _ = writeln!(std::io::stderr().lock(), "{message}");
    }));
    install_route_mutation_panic_hook_at_startup();
    let _ = std::panic::catch_unwind(|| panic!("LOAD_FAILURE_UNRELATED_SENTINEL_109cbc43"));
    runtime.block_on(async {
        let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
        let registry = RouteRegistry::load(store.clone()).await.unwrap();
        let mutation = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .put(key("/load-hook"), route("http://load-hook.example"))
                    .await
            }
        });
        store.entered.acquire().await.unwrap().forget();
        store.release.add_permits(1);
        assert_eq!(
            mutation.await.unwrap().unwrap_err().to_string(),
            "route put mutation task panicked"
        );
    });
}

async fn start_and_cancel_supervised_put(
    registry: &Arc<RouteRegistry>,
    store: &Arc<SupervisedPutStore>,
    route_key: RouteKey,
    route_data: RouteData,
) {
    let caller = {
        let registry = Arc::clone(registry);
        tokio::spawn(async move { registry.put(route_key, route_data).await })
    };
    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
}

#[derive(Default)]
struct AdmissionCountingStore {
    mutation_calls: AtomicUsize,
}

#[async_trait]
impl Store for AdmissionCountingStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(BTreeMap::new())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(route("http://unexpected-add.example"))
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(data)
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

struct FirstMutationPanicStore {
    mutation_calls: AtomicUsize,
    first_entered: Semaphore,
    release_first: Semaphore,
}

impl FirstMutationPanicStore {
    fn new() -> Self {
        Self {
            mutation_calls: AtomicUsize::new(0),
            first_entered: Semaphore::new(0),
            release_first: Semaphore::new(0),
        }
    }

    fn enter(&self) -> bool {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst) == 0
    }
}

#[async_trait]
impl Store for FirstMutationPanicStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(BTreeMap::new())
    }

    async fn add(
        &self,
        _key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        assert!(!self.enter(), "the first store mutation must be put");
        let mut data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        Ok(data)
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        if self.enter() {
            self.first_entered.add_permits(1);
            self.release_first.acquire().await.unwrap().forget();
            std::panic::panic_any(PanickingPayloadDrop);
        }
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        if self.enter() {
            self.first_entered.add_permits(1);
            self.release_first.acquire().await.unwrap().forget();
            std::panic::panic_any(PanickingPayloadDrop);
        }
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        Ok(data)
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        assert!(!self.enter(), "the first store mutation must be put");
        Ok(())
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        assert!(!self.enter(), "the first store mutation must be put");
        Ok(None)
    }
}

#[tokio::test]
async fn first_backend_panic_terminally_seals_queued_and_later_mutations() {
    let store = Arc::new(FirstMutationPanicStore::new());
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let first = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .put(key("/panic/first"), route("http://panic-first.example"))
                .await
                .map(|_| ())
        }
    });
    store.first_entered.acquire().await.unwrap().forget();

    let queued = [
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .add(
                        key("/panic/queued-add"),
                        "http://queued-add.example".to_owned(),
                        Map::new(),
                    )
                    .await
                    .map(|_| ())
            }
        }),
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .put(key("/panic/queued-put"), route("http://queued-put.example"))
                    .await
                    .map(|_| ())
            }
        }),
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .update_activity(&key("/panic/queued-activity"), Utc::now())
                    .await
                    .map(|_| ())
            }
        }),
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .delete(&key("/panic/queued-delete"))
                    .await
                    .map(|_| ())
            }
        }),
    ];

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if registry.mutation_status().active_mutations == 5 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    store.release_first.add_permits(1);

    assert_eq!(
        first.await.unwrap().unwrap_err().to_string(),
        "route put mutation task panicked"
    );
    for mutation in queued {
        assert_eq!(
            mutation.await.unwrap().unwrap_err().to_string(),
            MUTATION_PANIC_SEALED_ERROR
        );
    }

    let status = registry.mutation_status();
    assert!(status.sealed);
    assert_eq!(status.seal, MutationSeal::BackendPanic);
    assert_eq!(status.active_mutations, 0);

    let later_key = key("/panic/later");
    let later_errors = [
        registry
            .add(
                later_key.clone(),
                "http://later-add.example".to_owned(),
                Map::new(),
            )
            .await
            .unwrap_err(),
        registry
            .put(later_key.clone(), route("http://later-put.example"))
            .await
            .unwrap_err(),
        registry
            .update_activity(&later_key, Utc::now())
            .await
            .unwrap_err(),
        registry.delete(&later_key).await.unwrap_err(),
    ];
    assert!(later_errors
        .iter()
        .all(|error| error.to_string() == MUTATION_PANIC_SEALED_ERROR));

    let drained = registry.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(drained.detached_failures.is_empty());
    assert!(drained.detached_panics.is_empty());
    assert_eq!(registry.mutation_status().seal, MutationSeal::BackendPanic);
    assert_eq!(store.mutation_calls.load(Ordering::SeqCst), 1);
    assert!(registry.all().is_empty());
}

#[tokio::test]
async fn seal_wins_before_begin_and_rejects_every_mutation_without_store_or_publication() {
    let store = Arc::new(AdmissionCountingStore::default());
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    let drained = registry.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    let status = registry.mutation_status();
    assert!(status.sealed);
    assert_eq!(status.active_mutations, 0);

    let route_key = key("/sealed");
    let errors = [
        registry
            .add(
                route_key.clone(),
                "http://sealed-add.example".to_owned(),
                Map::new(),
            )
            .await
            .unwrap_err(),
        registry
            .put(route_key.clone(), route("http://sealed-put.example"))
            .await
            .unwrap_err(),
        registry
            .update_activity(&route_key, Utc::now())
            .await
            .unwrap_err(),
        registry.delete(&route_key).await.unwrap_err(),
    ];
    assert!(errors
        .iter()
        .all(|error| error.to_string() == MUTATION_ADMISSION_SEALED_ERROR));
    assert_eq!(store.mutation_calls.load(Ordering::SeqCst), 0);
    assert!(registry.all().is_empty());
}

#[tokio::test]
async fn begin_wins_before_seal_then_drains_while_later_mutations_are_rejected() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let begun = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .put(key("/begun"), route("http://begun.example"))
                .await
        }
    });
    store.entered.acquire().await.unwrap().forget();
    let before_seal = registry.mutation_status();
    assert!(!before_seal.sealed);
    assert_eq!(before_seal.active_mutations, 1);

    let draining = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.drain_mutations(Duration::from_secs(1)).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if registry.mutation_status().sealed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rejected = registry
        .put(key("/late"), route("http://late.example"))
        .await
        .unwrap_err();
    assert_eq!(rejected.to_string(), MUTATION_ADMISSION_SEALED_ERROR);

    store.release.add_permits(1);
    begun.await.unwrap().unwrap();
    let drained = draining.await.unwrap();
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(registry.get(&key("/begun")).is_some());
    assert!(registry.get(&key("/late")).is_none());
}

#[tokio::test]
async fn accepted_but_unpolled_handler_is_rejected_after_drain_seals_admission() {
    let store = Arc::new(AdmissionCountingStore::default());
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let accepted = registry.put(key("/accepted"), route("http://accepted.example"));

    let drained = registry.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    let error = accepted.await.unwrap_err();
    assert_eq!(error.to_string(), MUTATION_ADMISSION_SEALED_ERROR);
    assert_eq!(store.mutation_calls.load(Ordering::SeqCst), 0);
    assert!(registry.get(&key("/accepted")).is_none());
}

#[tokio::test]
async fn cancelled_caller_later_success_converges_and_drain_succeeds() {
    let route_key = key("/supervised-success");
    let route_data = route("http://success.example");
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    start_and_cancel_supervised_put(&registry, &store, route_key.clone(), route_data.clone()).await;
    store.release.add_permits(1);

    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(drained.detached_failures.is_empty());
    assert!(drained.detached_panics.is_empty());
    assert_eq!(registry.get(&route_key), Some(route_data));
}

#[tokio::test]
async fn cancelled_caller_later_store_error_is_surfaced_by_drain() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Error));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    start_and_cancel_supervised_put(
        &registry,
        &store,
        key("/supervised-error"),
        route("http://error.example"),
    )
    .await;
    let while_active = registry.drain_mutations(Duration::ZERO).await;
    assert!(while_active.timed_out);
    assert_eq!(while_active.active_mutations, 1);
    assert!(while_active.detached_failures.is_empty());
    store.release.add_permits(1);

    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert_eq!(drained.detached_failures.len(), 1);
    assert_eq!(
        drained.detached_failures[0].operation,
        MutationOperation::Put
    );
    assert_eq!(
        drained.detached_failures[0].error.to_string(),
        "detached backend failure"
    );
    assert!(!format!("{drained:?}").contains("detached backend failure"));
    assert!(drained.detached_panics.is_empty());
}

#[tokio::test]
async fn drain_times_out_while_backend_pending_then_succeeds_after_release() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    start_and_cancel_supervised_put(
        &registry,
        &store,
        key("/pending"),
        route("http://pending.example"),
    )
    .await;

    let timed_out = registry.drain_mutations(Duration::from_millis(10)).await;
    assert!(timed_out.timed_out);
    assert_eq!(timed_out.active_mutations, 1);

    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
}

#[tokio::test]
async fn zero_duration_drain_distinguishes_inactive_from_active() {
    let inactive = RouteRegistry::load(memory_store()).await.unwrap();
    let drained = inactive.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);

    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let active = RouteRegistry::load(store.clone()).await.unwrap();
    start_and_cancel_supervised_put(
        &active,
        &store,
        key("/zero-active"),
        route("http://zero-active.example"),
    )
    .await;

    let timed_out = active.drain_mutations(Duration::ZERO).await;
    assert!(timed_out.timed_out);
    assert_eq!(timed_out.active_mutations, 1);

    store.release.add_permits(1);
    assert!(
        !active
            .drain_mutations(Duration::from_secs(1))
            .await
            .timed_out
    );
}

#[tokio::test]
async fn completion_at_drain_deadline_never_reports_timeout_with_zero_active() {
    for index in 0..128 {
        let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
        let registry = RouteRegistry::load(store.clone()).await.unwrap();
        start_and_cancel_supervised_put(
            &registry,
            &store,
            key(&format!("/deadline/{index}")),
            route("http://deadline.example"),
        )
        .await;

        let release = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                tokio::task::yield_now().await;
                store.release.add_permits(1);
            }
        });
        let outcome = registry.drain_mutations(Duration::ZERO).await;
        assert!(!(outcome.timed_out && outcome.active_mutations == 0));
        release.await.unwrap();
        if outcome.active_mutations != 0 {
            let completed = registry.drain_mutations(Duration::from_secs(1)).await;
            assert!(!completed.timed_out);
            assert_eq!(completed.active_mutations, 0);
        }
    }
}

#[tokio::test]
async fn detached_diagnostics_are_bounded_and_consumed_once() {
    let total = DETACHED_MUTATION_DIAGNOSTIC_CAPACITY + 37;
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Error));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let callers: Vec<_> = (0..total)
        .map(|index| {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                registry
                    .put(
                        key(&format!("/overflow/{index}")),
                        route("http://overflow.example"),
                    )
                    .await
            })
        })
        .collect();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = registry.mutation_status();
            if status.active_mutations == total {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for caller in &callers {
        caller.abort();
    }
    for caller in callers {
        assert!(caller.await.unwrap_err().is_cancelled());
    }
    store.release.add_permits(total);

    let drained = registry.drain_mutations(Duration::from_secs(5)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert_eq!(
        drained.detached_failures.len() + drained.detached_panics.len(),
        DETACHED_MUTATION_DIAGNOSTIC_CAPACITY
    );
    assert_eq!(
        drained.dropped_detached_failures,
        total - DETACHED_MUTATION_DIAGNOSTIC_CAPACITY
    );
    assert_eq!(drained.dropped_detached_panics, 0);

    let consumed = registry.drain_mutations(Duration::ZERO).await;
    assert!(!consumed.timed_out);
    assert!(consumed.detached_failures.is_empty());
    assert!(consumed.detached_panics.is_empty());
    assert_eq!(consumed.dropped_detached_failures, 0);
    assert_eq!(consumed.dropped_detached_panics, 0);
    assert!(registry.mutation_status().sealed);
    assert_eq!(
        registry
            .put(key("/after-repeat"), route("http://after-repeat.example"))
            .await
            .unwrap_err()
            .to_string(),
        MUTATION_ADMISSION_SEALED_ERROR
    );
}

#[tokio::test]
async fn multiple_concurrent_mutations_drain_and_registry_lifetime_is_released() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let lifetime = Arc::downgrade(&registry);
    let callers: Vec<_> = (0..16)
        .map(|index| {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                registry
                    .put(
                        key(&format!("/concurrent/{index}")),
                        route(&format!("http://concurrent-{index}.example")),
                    )
                    .await
            })
        })
        .collect();

    store.entered.acquire().await.unwrap().forget();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if registry.mutation_status().active_mutations == 16 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let draining = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.drain_mutations(Duration::from_secs(1)).await })
    };
    store.release.add_permits(16);
    let drained = draining.await.unwrap();
    assert!(!drained.timed_out);
    for caller in callers {
        caller.await.unwrap().unwrap();
    }
    assert_eq!(registry.all().len(), 16);

    drop(registry);
    assert!(lifetime.upgrade().is_none());
}

#[test]
fn mutation_without_a_runtime_fails_and_leaves_the_tracker_inactive() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime
        .block_on(RouteRegistry::load(memory_store()))
        .unwrap();
    drop(runtime);

    let mut mutation =
        Box::pin(registry.put(key("/no-runtime"), route("http://no-runtime.example")));
    let mut context = Context::from_waker(Waker::noop());
    let Poll::Ready(result) = mutation.as_mut().poll(&mut context) else {
        panic!("missing-runtime failure must be immediate");
    };
    assert_eq!(
        result.unwrap_err().to_string(),
        "route put mutation could not start: Tokio runtime unavailable"
    );
    drop(mutation);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let drained = runtime.block_on(registry.drain_mutations(Duration::from_millis(10)));
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(drained.detached_failures.is_empty());
    assert!(drained.detached_panics.is_empty());
}

#[test]
fn terminal_seal_takes_precedence_over_runtime_availability() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime
        .block_on(RouteRegistry::load(memory_store()))
        .unwrap();
    let drained = runtime.block_on(registry.drain_mutations(Duration::ZERO));
    assert!(!drained.timed_out);
    drop(runtime);

    let mut mutation =
        Box::pin(registry.put(key("/sealed-no-runtime"), route("http://sealed.example")));
    let mut context = Context::from_waker(Waker::noop());
    let Poll::Ready(result) = mutation.as_mut().poll(&mut context) else {
        panic!("sealed mutation rejection must be immediate");
    };
    assert_eq!(
        result.unwrap_err().to_string(),
        MUTATION_ADMISSION_SEALED_ERROR
    );
}

#[test]
fn runtime_shutdown_does_not_leak_active_count_or_registry_lifetime() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = runtime
        .block_on(RouteRegistry::load(store.clone()))
        .unwrap();
    let lifetime = Arc::downgrade(&registry);
    let caller = runtime.spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .put(key("/runtime-shutdown"), route("http://shutdown.example"))
                .await
        }
    });
    runtime.block_on(async { store.entered.acquire().await.unwrap().forget() });

    drop(runtime);
    drop(caller);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let drained = runtime.block_on(registry.drain_mutations(Duration::from_secs(1)));
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert_eq!(drained.detached_panics.len(), 1);
    assert_eq!(drained.detached_panics[0].operation, MutationOperation::Put);
    assert_eq!(
        drained.detached_panics[0].message,
        "route put mutation task terminated unexpectedly"
    );

    drop(registry);
    assert!(lifetime.upgrade().is_none());
}
