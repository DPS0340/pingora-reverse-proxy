use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use pingora_reverse_proxy::route::{RouteData, RouteKey};
use pingora_reverse_proxy::route_table::{RouteMatch, RouteRegistry};
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::{Store, StoreError};
use proptest::prelude::*;
use serde_json::{json, Map};
use tokio::sync::{Barrier, RwLock, Semaphore};
use url::Url;

fn key(path: &str) -> RouteKey {
    RouteKey::parse(path).unwrap()
}

fn route(target: &str) -> RouteData {
    RouteData {
        target: Url::parse(target).unwrap(),
        last_activity: Utc.timestamp_opt(1, 0).unwrap(),
        extra: Map::from_iter([("owner".to_owned(), json!("jupyterhub"))]),
    }
}

fn memory_store() -> Arc<dyn Store> {
    Arc::new(MemoryStore::new())
}

async fn assert_store_contract(store: Arc<dyn Store>) {
    let route_key = key("//user/alice///");
    let original = route("http://127.0.0.1:9000/base");
    store
        .put(route_key.clone(), original.clone())
        .await
        .unwrap();
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&original)
    );

    let replacement = route("http://127.0.0.1:9001/replaced");
    store
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();
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
                        store.put(route_key.clone(), route_data.clone()).await.unwrap();
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

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
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

struct GatedPutStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    gates: BTreeMap<RouteKey, Arc<Semaphore>>,
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

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.entries.lock().unwrap().push(key.clone());
        self.gates[&key].acquire().await.unwrap().forget();
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }
}

fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

#[tokio::test]
async fn concurrent_writers_are_serialized_without_losing_mutations() {
    let first_key = key("/first");
    let second_key = key("/second");
    let store = Arc::new(GatedPutStore::new([first_key.clone(), second_key.clone()]));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let first_data = route("http://127.0.0.1:9001/first");
    let second_data = route("http://127.0.0.1:9002/second");

    let mut first = Box::pin(registry.put(first_key.clone(), first_data.clone()));
    let mut second = Box::pin(registry.put(second_key.clone(), second_data.clone()));

    assert!(poll_once(first.as_mut()).is_pending());
    assert_eq!(store.entries(), vec![first_key.clone()]);
    assert!(poll_once(second.as_mut()).is_pending());
    assert_eq!(
        store.entries(),
        vec![first_key.clone()],
        "the second writer must not clone a stale snapshot or enter persistence"
    );

    store.release(&first_key);
    assert!(matches!(poll_once(first.as_mut()), Poll::Ready(Ok(()))));
    assert!(poll_once(second.as_mut()).is_pending());
    assert_eq!(store.entries(), vec![first_key.clone(), second_key.clone()]);
    store.release(&second_key);
    assert!(matches!(poll_once(second.as_mut()), Poll::Ready(Ok(()))));

    let expected = BTreeMap::from([
        (first_key.clone(), first_data.clone()),
        (second_key.clone(), second_data.clone()),
    ]);
    assert_eq!(registry.all(), expected);
    assert_eq!(registry.get(&first_key), Some(first_data));
    assert_eq!(registry.get(&second_key), Some(second_data));
    assert_eq!(registry.resolve("/first/request").unwrap().key, first_key);
    assert_eq!(registry.resolve("/second/request").unwrap().key, second_key);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_observe_only_complete_snapshots() {
    let registry = Arc::new(RouteRegistry::load(memory_store()).await.unwrap());
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
