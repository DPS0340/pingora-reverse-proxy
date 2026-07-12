//! Immutable, segment-indexed route matching.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;

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

/// Registry mutation kind reported by lifecycle diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationOperation {
    Add,
    Put,
    UpdateActivity,
    Delete,
}

impl MutationOperation {
    fn name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Put => "put",
            Self::UpdateActivity => "activity update",
            Self::Delete => "delete",
        }
    }

    fn panic_message(self) -> String {
        format!("route {} mutation task panicked", self.name())
    }
}

/// A backend error whose caller stopped waiting before it completed.
pub struct DetachedMutationFailure {
    pub operation: MutationOperation,
    pub error: StoreError,
}

impl std::fmt::Debug for DetachedMutationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DetachedMutationFailure")
            .field("operation", &self.operation)
            .field("error", &"[redacted StoreError]")
            .finish()
    }
}

/// A task panic whose caller stopped waiting before it completed.
#[derive(Debug, Eq, PartialEq)]
pub struct DetachedMutationPanic {
    pub operation: MutationOperation,
    /// Fixed text that deliberately excludes the panic payload and mutation data.
    pub message: String,
}

/// Bounded mutation-drain result and diagnostics accumulated since the last drain.
#[derive(Debug)]
pub struct MutationDrainOutcome {
    pub timed_out: bool,
    pub active_mutations: usize,
    pub detached_failures: Vec<DetachedMutationFailure>,
    pub detached_panics: Vec<DetachedMutationPanic>,
}

#[derive(Default)]
struct MutationTrackerState {
    active: usize,
    handles: Vec<TrackedMutation>,
    detached_failures: Vec<DetachedMutationFailure>,
    detached_panics: Vec<DetachedMutationPanic>,
}

struct TrackedMutation {
    handle: JoinHandle<()>,
}

#[derive(Default)]
struct MutationTracker {
    state: StdMutex<MutationTrackerState>,
    changed: Notify,
}

impl MutationTracker {
    fn state(&self) -> StdMutexGuard<'_, MutationTrackerState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn start(&self) {
        let mut state = self.state();
        state.active = state.active.saturating_add(1);
    }

    fn track(&self, handle: JoinHandle<()>) {
        self.state().handles.push(TrackedMutation { handle });
    }

    fn finish(
        &self,
        failure: Option<DetachedMutationFailure>,
        panic: Option<DetachedMutationPanic>,
    ) {
        {
            let mut state = self.state();
            state.active = state.active.saturating_sub(1);
            if let Some(failure) = failure {
                state.detached_failures.push(failure);
            }
            if let Some(panic) = panic {
                state.detached_panics.push(panic);
            }
        }
        self.changed.notify_waiters();
    }

    fn active(&self) -> usize {
        self.state().active
    }

    async fn wait_until_inactive(&self) {
        loop {
            let changed = self.changed.notified();
            if self.active() == 0 {
                return;
            }
            changed.await;
        }
    }

    fn outcome(&self, timed_out: bool) -> MutationDrainOutcome {
        let mut state = self.state();
        state
            .handles
            .retain(|tracked| !tracked.handle.is_finished());
        MutationDrainOutcome {
            timed_out,
            active_mutations: state.active,
            detached_failures: std::mem::take(&mut state.detached_failures),
            detached_panics: std::mem::take(&mut state.detached_panics),
        }
    }
}

struct ActiveMutation {
    tracker: Arc<MutationTracker>,
    operation: MutationOperation,
    finished: bool,
}

impl ActiveMutation {
    fn new(tracker: Arc<MutationTracker>, operation: MutationOperation) -> Self {
        Self {
            tracker,
            operation,
            finished: false,
        }
    }

    fn finish(
        mut self,
        failure: Option<DetachedMutationFailure>,
        panic: Option<DetachedMutationPanic>,
    ) {
        self.finished = true;
        self.tracker.finish(failure, panic);
    }
}

impl Drop for ActiveMutation {
    fn drop(&mut self) {
        if !self.finished {
            self.tracker.finish(
                None,
                Some(DetachedMutationPanic {
                    operation: self.operation,
                    message: format!(
                        "route {} mutation task terminated unexpectedly",
                        self.operation.name()
                    ),
                }),
            );
        }
    }
}

struct CatchUnwindFuture<F> {
    future: Pin<Box<F>>,
}

struct MutationResponse<T> {
    result: Result<T, StoreError>,
    acknowledged: oneshot::Sender<()>,
}

impl<F> CatchUnwindFuture<F> {
    fn new(future: F) -> Self {
        Self {
            future: Box::pin(future),
        }
    }
}

impl<F: Future> Future for CatchUnwindFuture<F> {
    type Output = Result<F::Output, ()>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| self.future.as_mut().poll(context))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
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
///
/// Each mutation runs in a registry-owned Tokio task. Cancelling the caller
/// detaches its wait for the result but does not cancel persistence, snapshot
/// reconciliation, or publication. The task retains the registry until the
/// backend operation finishes; backends therefore need finite operational
/// timeouts. Normal timeout errors release both the mutation lock and retained
/// registry reference. Tokio runtime shutdown may cancel outstanding tasks, so
/// backend mutations must still be atomic at their own persistence boundary.
pub struct RouteRegistry {
    store: Arc<dyn Store>,
    snapshot: ArcSwap<RouteSnapshot>,
    mutation: Mutex<()>,
    mutations: Arc<MutationTracker>,
}

impl RouteRegistry {
    /// Load the complete persisted map before publication.
    ///
    /// Because persisted snapshots do not contain runtime mutation history,
    /// initial matcher precedence is deterministic ascending `RouteKey` order.
    pub async fn load(store: Arc<dyn Store>) -> Result<Arc<Self>, StoreError> {
        let routes = store.snapshot().await?;
        Ok(Arc::new(Self {
            store,
            snapshot: ArcSwap::from_pointee(RouteSnapshot::from_routes(routes)),
            mutation: Mutex::new(()),
            mutations: Arc::new(MutationTracker::default()),
        }))
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
    pub async fn put(self: &Arc<Self>, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        let registry = Arc::clone(self);
        self.run_mutation(MutationOperation::Put, async move {
            registry.put_owned(key, data).await
        })
        .await
    }

    async fn put_owned(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        let _mutation = self.mutation.lock().await;
        let mut routes = self.snapshot.load().routes.clone();
        self.store.put(key.clone(), data.clone()).await?;
        routes.replace(key, data);
        self.publish(routes);
        Ok(())
    }

    /// Publish only the record atomically committed and stamped by the backend.
    pub async fn add(
        self: &Arc<Self>,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
    ) -> Result<(), StoreError> {
        let registry = Arc::clone(self);
        self.run_mutation(MutationOperation::Add, async move {
            registry.add_owned(key, target, extra).await
        })
        .await
    }

    async fn add_owned(
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
        self: &Arc<Self>,
        key: &RouteKey,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let registry = Arc::clone(self);
        let key = key.clone();
        self.run_mutation(MutationOperation::UpdateActivity, async move {
            registry.update_activity_owned(key, at).await
        })
        .await
    }

    async fn update_activity_owned(
        &self,
        key: RouteKey,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let _mutation = self.mutation.lock().await;
        let mut routes = self.snapshot.load().routes.clone();
        self.store.update_activity(&key, at).await?;
        routes.update_activity(&key, at);
        self.publish(routes);
        Ok(())
    }

    /// Persist deletion and return the record reported by the backing store.
    pub async fn delete(self: &Arc<Self>, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        let registry = Arc::clone(self);
        let key = key.clone();
        self.run_mutation(MutationOperation::Delete, async move {
            registry.delete_owned(key).await
        })
        .await
    }

    async fn delete_owned(&self, key: RouteKey) -> Result<Option<RouteData>, StoreError> {
        let _mutation = self.mutation.lock().await;
        let mut routes = self.snapshot.load().routes.clone();
        let deleted = self.store.delete(&key).await?;
        routes.remove(&key);
        self.publish(routes);
        Ok(deleted)
    }

    fn publish(&self, routes: OrderedRoutes) {
        self.snapshot
            .store(Arc::new(RouteSnapshot::from_ordered_routes(routes)));
    }

    /// Wait at most `timeout` for all currently accepted mutations to finish.
    ///
    /// Detached diagnostics are returned once and consumed by this call. A
    /// timeout does not cancel pending mutations; a later drain can finish them.
    pub async fn drain_mutations(&self, timeout: Duration) -> MutationDrainOutcome {
        let timed_out = tokio::time::timeout(timeout, async {
            self.mutations.wait_until_inactive().await;
            // Completion accounting is the task's final synchronous action.
            // Yield once so completed handles can transition to `is_finished`
            // before the tracker prunes them without detaching pending work.
            tokio::task::yield_now().await;
        })
        .await
        .is_err();

        self.mutations.outcome(timed_out)
    }

    async fn run_mutation<T, F>(
        &self,
        operation: MutationOperation,
        mutation: F,
    ) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, StoreError>> + Send + 'static,
    {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            StoreError::message(format!(
                "route {} mutation could not start: Tokio runtime unavailable",
                operation.name()
            ))
        })?;
        let (response, receiver) = oneshot::channel();
        self.mutations.start();
        let active = ActiveMutation::new(Arc::clone(&self.mutations), operation);
        let handle = runtime.spawn(async move {
            match CatchUnwindFuture::new(mutation).await {
                Ok(result) => {
                    let detached_error = result.as_ref().err().cloned();
                    let (acknowledged, acknowledgment) = oneshot::channel();
                    let detached = match response.send(MutationResponse {
                        result,
                        acknowledged,
                    }) {
                        Ok(()) => acknowledgment.await.is_err(),
                        Err(_) => true,
                    };
                    let detached_failure = detached_error
                        .filter(|_| detached)
                        .map(|error| DetachedMutationFailure { operation, error });
                    active.finish(detached_failure, None);
                }
                Err(()) => {
                    let message = operation.panic_message();
                    let (acknowledged, acknowledgment) = oneshot::channel();
                    let detached = match response.send(MutationResponse {
                        result: Err(StoreError::message(message.clone())),
                        acknowledged,
                    }) {
                        Ok(()) => acknowledgment.await.is_err(),
                        Err(_) => true,
                    };
                    let detached_panic =
                        detached.then_some(DetachedMutationPanic { operation, message });
                    active.finish(None, detached_panic);
                }
            }
        });
        self.mutations.track(handle);

        let response = receiver.await.map_err(|_| {
            StoreError::message(format!(
                "route {} mutation task terminated before returning a result",
                operation.name()
            ))
        })?;
        let _ = response.acknowledged.send(());
        response.result
    }
}

fn segments(path: &str) -> impl Iterator<Item = &str> {
    let path = path.trim_matches('/');
    path.split('/').filter(move |_| !path.is_empty())
}
