//! Immutable, segment-indexed route matching.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::route::{RouteData, RouteKey};
use crate::store::{Store, StoreError};

#[derive(Clone, Debug, Default)]
struct Node {
    children: BTreeMap<String, Node>,
    route: Option<RouteMatch>,
}

#[derive(Clone, Debug, Default)]
struct OrderedRoutes {
    by_key: BTreeMap<RouteKey, RouteData>,
    mutation_order: Vec<RouteKey>,
}

impl OrderedRoutes {
    fn from_sorted(by_key: BTreeMap<RouteKey, RouteData>) -> Self {
        let mutation_order = by_key.keys().cloned().collect();
        Self {
            by_key,
            mutation_order,
        }
    }

    fn replace(&mut self, key: RouteKey, data: RouteData) {
        self.mutation_order.retain(|existing| existing != &key);
        self.mutation_order.push(key.clone());
        self.by_key.insert(key, data);
    }

    fn update_activity(&mut self, key: &RouteKey, at: DateTime<Utc>) {
        if let Some(route) = self.by_key.get_mut(key) {
            route.last_activity = at;
        }
    }

    fn remove(&mut self, key: &RouteKey) {
        self.by_key.remove(key);
        self.mutation_order.retain(|existing| existing != key);
    }
}

/// A matched normalized key and shared immutable route data.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteMatch {
    pub key: RouteKey,
    pub data: Arc<RouteData>,
}

/// An immutable snapshot optimized for segment-prefix lookups.
#[derive(Clone, Debug, Default)]
pub struct RouteSnapshot {
    root: Node,
    routes: OrderedRoutes,
}

impl RouteSnapshot {
    /// Build a matcher using ascending `RouteKey` order.
    ///
    /// This deterministic order is also used for initial store loads, where
    /// no successful runtime mutation history is available.
    pub fn from_routes(routes: BTreeMap<RouteKey, RouteData>) -> Self {
        Self::from_ordered_routes(OrderedRoutes::from_sorted(routes))
    }

    fn from_ordered_routes(routes: OrderedRoutes) -> Self {
        let mut root = Node::default();

        for key in &routes.mutation_order {
            let Some(data) = routes.by_key.get(key) else {
                debug_assert!(false, "ordered route key must exist in the route map");
                continue;
            };
            let mut node = &mut root;
            for segment in segments(key.as_str()) {
                node = node.children.entry(segment.to_owned()).or_default();
            }
            node.route = Some(RouteMatch {
                key: key.clone(),
                data: Arc::new(data.clone()),
            });
        }

        Self { root, routes }
    }

    /// Resolve a request path to the deepest stored route on segment boundaries.
    pub fn resolve(&self, request_path: &str) -> Option<RouteMatch> {
        let mut node = &self.root;
        let mut matched = node.route.as_ref();

        for segment in segments(request_path) {
            let Some(child) = node.children.get(segment) else {
                break;
            };
            node = child;
            if node.route.is_some() {
                matched = node.route.as_ref();
            }
        }

        matched.cloned()
    }
}

/// Persistence-backed registry with lock-free reads from immutable snapshots.
pub struct RouteRegistry {
    store: Arc<dyn Store>,
    snapshot: ArcSwap<RouteSnapshot>,
    mutation: Mutex<()>,
}

impl RouteRegistry {
    /// Load the complete persisted map before publication.
    ///
    /// Because persisted snapshots do not contain runtime mutation history,
    /// initial matcher precedence is deterministic ascending `RouteKey` order.
    pub async fn load(store: Arc<dyn Store>) -> Result<Self, StoreError> {
        let routes = store.snapshot().await?;
        Ok(Self {
            store,
            snapshot: ArcSwap::from_pointee(RouteSnapshot::from_routes(routes)),
            mutation: Mutex::new(()),
        })
    }

    /// Return one normalized route record from the current snapshot.
    pub fn get(&self, key: &RouteKey) -> Option<RouteData> {
        self.snapshot.load().routes.by_key.get(key).cloned()
    }

    /// Return a complete clone of the current logical route map.
    pub fn all(&self) -> BTreeMap<RouteKey, RouteData> {
        self.snapshot.load().routes.by_key.clone()
    }

    /// Resolve a runtime request path against the current immutable snapshot.
    pub fn resolve(&self, request_path: &str) -> Option<RouteMatch> {
        self.snapshot.load().resolve(request_path)
    }

    /// Persist a route replacement and publish it atomically on success.
    pub async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        let _mutation = self.mutation.lock().await;
        let mut routes = self.snapshot.load().routes.clone();
        self.store.put(key.clone(), data.clone()).await?;
        routes.replace(key, data);
        self.publish(routes);
        Ok(())
    }

    /// Publish only the record atomically committed and stamped by the backend.
    pub async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
    ) -> Result<(), StoreError> {
        let _mutation = self.mutation.lock().await;
        let mut routes = self.snapshot.load().routes.clone();
        let data = self.store.add(key.clone(), target, extra).await?;
        routes.replace(key, data);
        self.publish(routes);
        Ok(())
    }

    /// Persist an activity timestamp while retaining all other route fields.
    pub async fn update_activity(
        &self,
        key: &RouteKey,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let _mutation = self.mutation.lock().await;
        let mut routes = self.snapshot.load().routes.clone();
        self.store.update_activity(key, at).await?;
        routes.update_activity(key, at);
        self.publish(routes);
        Ok(())
    }

    /// Persist deletion and return the record reported by the backing store.
    pub async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        let _mutation = self.mutation.lock().await;
        let mut routes = self.snapshot.load().routes.clone();
        let deleted = self.store.delete(key).await?;
        routes.remove(key);
        self.publish(routes);
        Ok(deleted)
    }

    fn publish(&self, routes: OrderedRoutes) {
        self.snapshot
            .store(Arc::new(RouteSnapshot::from_ordered_routes(routes)));
    }
}

fn segments(path: &str) -> impl Iterator<Item = &str> {
    let path = path.trim_matches('/');
    path.split('/').filter(move |_| !path.is_empty())
}
