use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use pingora_reverse_proxy::route::{RouteData, RouteKey};
use pingora_reverse_proxy::route_table::{MutationOperation, RouteMatch, RouteRegistry};
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::{Store, StoreError};
use proptest::prelude::*;
use serde_json::{json, Map};
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
        )
        .await
        .unwrap();
    assert!(added.last_activity >= add_started_at);
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&added),
        "add must return the exact atomically committed record"
    );

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

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("injected add failure"))
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
    ) -> Result<RouteData, StoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        if self.fail {
            return Err(StoreError::message("injected atomic add failure"));
        }

        let mut routes = self.routes.write().await;
        let data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.routes.write().await.insert(key, data);
        Ok(())
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
    ) -> Result<RouteData, StoreError> {
        let data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.gate(GatedMutation::Put).await;
        self.routes.write().await.insert(key, data);
        Ok(())
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
async fn cancelling_activity_update_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let activity = Utc.timestamp_opt(999, 0).unwrap();
    let store = Arc::new(GatedMutationStore::new(
        GatedMutation::UpdateActivity,
        BTreeMap::from([(route_key.clone(), original)]),
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let caller = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move { registry.update_activity(&route_key, activity).await })
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
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("unused add"))
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
            .await
            .map_err(|_| StoreError::message("backend timeout"))
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
            SupervisedPutOutcome::Panic => panic!("sensitive panic payload must not escape"),
        }
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        Err(StoreError::message("unused activity update"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("unused delete"))
    }
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
async fn backend_panic_reaches_live_and_detached_callers_without_stranding_active_count() {
    let live_store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
    let live_registry = RouteRegistry::load(live_store.clone()).await.unwrap();
    let live = {
        let registry = Arc::clone(&live_registry);
        tokio::spawn(async move {
            registry
                .put(key("/live-panic"), route("http://live.example"))
                .await
        })
    };
    live_store.entered.acquire().await.unwrap().forget();
    live_store.release.add_permits(1);
    let error = live.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "route put mutation task panicked");
    let live_drain = live_registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!live_drain.timed_out);
    assert_eq!(live_drain.active_mutations, 0);
    assert!(live_drain.detached_panics.is_empty());

    let detached_store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
    let detached_registry = RouteRegistry::load(detached_store.clone()).await.unwrap();
    start_and_cancel_supervised_put(
        &detached_registry,
        &detached_store,
        key("/detached-panic"),
        route("http://detached.example"),
    )
    .await;
    detached_store.release.add_permits(1);
    let detached_drain = detached_registry
        .drain_mutations(Duration::from_secs(1))
        .await;
    assert!(!detached_drain.timed_out);
    assert_eq!(detached_drain.active_mutations, 0);
    assert_eq!(detached_drain.detached_panics.len(), 1);
    assert_eq!(
        detached_drain.detached_panics[0].operation,
        MutationOperation::Put
    );
    assert_eq!(
        detached_drain.detached_panics[0].message,
        "route put mutation task panicked"
    );
    assert!(!detached_drain.detached_panics[0]
        .message
        .contains("sensitive panic payload"));
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
            let probe = registry.drain_mutations(Duration::ZERO).await;
            if probe.active_mutations == 16 {
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
fn mutation_without_a_runtime_fails_without_incrementing_active_count() {
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
