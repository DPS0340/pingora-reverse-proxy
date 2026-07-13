//! Immutable, segment-indexed route matching.

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, Once};
use std::task::{Context, Poll};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::{oneshot, Mutex, Notify};

use crate::route::{RouteData, RouteKey};
use crate::store::{ActivityFloor, Store, StoreError};

static INSTALL_ROUTE_MUTATION_PANIC_HOOK: Once = Once::new();

thread_local! {
    static POLLING_SUPERVISED_MUTATION: Cell<bool> = const { Cell::new(false) };
}

/// Install the process hook used to redact supervised mutation panics at startup.
///
/// The application must call this once, after installing crash reporting and
/// before polling any route mutation. The installed hook captures and delegates
/// unrelated panics to the prior hook. It must remain the process hook for the
/// rest of the process lifetime and must not be replaced later.
///
/// If the application does not call this function, supervised mutations are
/// still caught and converted to fixed `StoreError`s, but the existing process
/// panic hook may expose their payload before `catch_unwind` returns.
pub fn install_route_mutation_panic_hook_at_startup() {
    INSTALL_ROUTE_MUTATION_PANIC_HOOK.call_once(|| {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            if !POLLING_SUPERVISED_MUTATION.get() {
                previous_hook(panic_info);
                return;
            }

            let mut stderr = std::io::stderr().lock();
            if let Some(location) = panic_info.location() {
                let _ = writeln!(
                    stderr,
                    "route mutation panic redacted at {}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                );
            } else {
                let _ = writeln!(
                    stderr,
                    "route mutation panic redacted at unknown source location"
                );
            }
        }));
    });
}

struct SupervisedMutationPollScope {
    previous: bool,
}

impl SupervisedMutationPollScope {
    fn enter() -> Self {
        let previous = POLLING_SUPERVISED_MUTATION.replace(true);
        Self { previous }
    }
}

impl Drop for SupervisedMutationPollScope {
    fn drop(&mut self) {
        POLLING_SUPERVISED_MUTATION.set(self.previous);
    }
}

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

    fn replace_preserving_activity(&mut self, key: RouteKey, mut data: RouteData) {
        if let Some(existing) = self.by_key.get(&key) {
            data.last_activity = data.last_activity.max(existing.last_activity);
        }
        self.replace(key, data);
    }

    fn update_activity(&mut self, key: &RouteKey, at: DateTime<Utc>) {
        if let Some(route) = self.by_key.get_mut(key) {
            route.last_activity = route.last_activity.max(at);
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
    /// Detached backend failures discarded when the diagnostic ring overflowed.
    pub dropped_detached_failures: usize,
    /// Detached panics discarded when the diagnostic ring overflowed.
    pub dropped_detached_panics: usize,
}

/// Non-consuming mutation admission state for readiness and tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MutationStatus {
    pub sealed: bool,
    pub seal: MutationSeal,
    pub active_mutations: usize,
}

/// Whether cached route state is authoritative enough to serve or expose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsistencyStatus {
    Consistent,
    Indeterminate,
}

/// Terminal mutation-admission state exposed by [`RouteRegistry::mutation_status`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationSeal {
    Open,
    Shutdown,
    BackendPanic,
    Indeterminate,
}

/// Fixed error returned by every mutation attempted after shutdown sealing.
pub const MUTATION_ADMISSION_SEALED_ERROR: &str =
    "route registry is shutting down; mutation admission is sealed";

/// Fixed error returned after a supervised backend panic terminally seals a registry.
pub const MUTATION_PANIC_SEALED_ERROR: &str =
    "route registry is terminally sealed after a backend panic";

/// Fixed error returned after an indeterminate backend mutation seals a registry.
pub const MUTATION_INDETERMINATE_SEALED_ERROR: &str =
    "route registry is terminally sealed after an indeterminate backend mutation";

/// Maximum detached mutation diagnostics retained between drains.
///
/// The single shared ring bounds total diagnostic retention across failures and
/// panics. On overflow, the oldest entry is discarded and counted in the next
/// drain outcome.
pub const DETACHED_MUTATION_DIAGNOSTIC_CAPACITY: usize = 256;

enum DetachedMutationDiagnostic {
    Failure(DetachedMutationFailure),
    Panic(DetachedMutationPanic),
}

struct MutationTrackerState {
    seal: MutationSeal,
    active: usize,
    diagnostics: VecDeque<DetachedMutationDiagnostic>,
    dropped_detached_failures: usize,
    dropped_detached_panics: usize,
}

impl Default for MutationTrackerState {
    fn default() -> Self {
        Self {
            seal: MutationSeal::Open,
            active: 0,
            diagnostics: VecDeque::with_capacity(DETACHED_MUTATION_DIAGNOSTIC_CAPACITY),
            dropped_detached_failures: 0,
            dropped_detached_panics: 0,
        }
    }
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

    fn begin(self: &Arc<Self>, operation: MutationOperation) -> Result<ActiveMutation, StoreError> {
        let mut state = self.state();
        match state.seal {
            MutationSeal::Open => {}
            MutationSeal::Shutdown => {
                return Err(StoreError::message(MUTATION_ADMISSION_SEALED_ERROR));
            }
            MutationSeal::BackendPanic => {
                return Err(StoreError::message(MUTATION_PANIC_SEALED_ERROR));
            }
            MutationSeal::Indeterminate => {
                return Err(StoreError::message(MUTATION_INDETERMINATE_SEALED_ERROR));
            }
        }
        state.active = state.active.saturating_add(1);
        drop(state);
        Ok(ActiveMutation {
            tracker: Arc::clone(self),
            operation,
            finished: false,
        })
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
                state.push_diagnostic(DetachedMutationDiagnostic::Failure(failure));
            }
            if let Some(panic) = panic {
                state.push_diagnostic(DetachedMutationDiagnostic::Panic(panic));
            }
        }
        self.changed.notify_waiters();
    }

    fn status(&self) -> MutationStatus {
        let state = self.state();
        MutationStatus {
            sealed: state.seal != MutationSeal::Open,
            seal: state.seal,
            active_mutations: state.active,
        }
    }

    fn panic_seal(&self) {
        self.state().seal = MutationSeal::BackendPanic;
    }

    fn indeterminate_seal(&self) {
        self.state().seal = MutationSeal::Indeterminate;
    }

    fn reject_if_terminally_failed(&self) -> Result<(), StoreError> {
        match self.state().seal {
            MutationSeal::BackendPanic => {
                return Err(StoreError::message(MUTATION_PANIC_SEALED_ERROR));
            }
            MutationSeal::Indeterminate => {
                return Err(StoreError::message(MUTATION_INDETERMINATE_SEALED_ERROR));
            }
            MutationSeal::Open | MutationSeal::Shutdown => {}
        }
        Ok(())
    }

    fn is_indeterminate(&self) -> bool {
        self.state().seal == MutationSeal::Indeterminate
    }

    async fn seal_and_drain(&self, timeout: Duration) -> MutationDrainOutcome {
        {
            let mut state = self.state();
            if state.seal == MutationSeal::Open {
                state.seal = MutationSeal::Shutdown;
            }
            if state.active == 0 {
                return state.take_outcome(false);
            }
        }

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.state();
                if state.active == 0 {
                    return state.take_outcome(false);
                }
            }
            tokio::select! {
                _ = &mut deadline => {
                    let mut state = self.state();
                    let timed_out = state.active != 0;
                    return state.take_outcome(timed_out);
                }
                _ = &mut changed => {}
            }
        }
    }
}

impl MutationTrackerState {
    fn take_outcome(&mut self, timed_out: bool) -> MutationDrainOutcome {
        let mut detached_failures = Vec::new();
        let mut detached_panics = Vec::new();
        for diagnostic in self.diagnostics.drain(..) {
            match diagnostic {
                DetachedMutationDiagnostic::Failure(failure) => {
                    detached_failures.push(failure);
                }
                DetachedMutationDiagnostic::Panic(panic) => detached_panics.push(panic),
            }
        }
        let dropped_detached_failures = std::mem::take(&mut self.dropped_detached_failures);
        let dropped_detached_panics = std::mem::take(&mut self.dropped_detached_panics);
        MutationDrainOutcome {
            timed_out,
            active_mutations: self.active,
            detached_failures,
            detached_panics,
            dropped_detached_failures,
            dropped_detached_panics,
        }
    }

    fn push_diagnostic(&mut self, diagnostic: DetachedMutationDiagnostic) {
        if self.diagnostics.len() == DETACHED_MUTATION_DIAGNOSTIC_CAPACITY {
            if let Some(dropped) = self.diagnostics.pop_front() {
                match dropped {
                    DetachedMutationDiagnostic::Failure(_) => {
                        self.dropped_detached_failures =
                            self.dropped_detached_failures.saturating_add(1);
                    }
                    DetachedMutationDiagnostic::Panic(_) => {
                        self.dropped_detached_panics =
                            self.dropped_detached_panics.saturating_add(1);
                    }
                }
            }
        }
        self.diagnostics.push_back(diagnostic);
    }
}

struct ActiveMutation {
    tracker: Arc<MutationTracker>,
    operation: MutationOperation,
    finished: bool,
}

impl ActiveMutation {
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
    tracker: Arc<MutationTracker>,
}

struct MutationResponse<T> {
    result: Result<T, StoreError>,
    acknowledged: oneshot::Sender<()>,
}

impl<F> CatchUnwindFuture<F> {
    fn new(future: F, tracker: Arc<MutationTracker>) -> Self {
        Self {
            future: Box::pin(future),
            tracker,
        }
    }
}

impl<F: Future> Future for CatchUnwindFuture<F> {
    type Output = Result<F::Output, ()>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| {
            let _panic_scope = SupervisedMutationPollScope::enter();
            self.future.as_mut().poll(context)
        })) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => {
                // The supervisor retains the mutation mutex outside this caught
                // future, so queued operations cannot pass their fence first.
                self.tracker.panic_seal();
                // A panic payload is arbitrary user/backend code. Dropping it can
                // panic again after the supervised poll scope has unwound, which
                // would bypass mutation redaction and may abort on a double panic.
                // Quarantine it instead; backend panics are terminal contract
                // violations and must never expose or execute an untrusted Drop.
                std::mem::forget(payload);
                Poll::Ready(Err(()))
            }
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
    mutation: Arc<Mutex<()>>,
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
            mutation: Arc::new(Mutex::new(())),
            mutations: Arc::new(MutationTracker::default()),
        }))
    }

    /// Return one normalized route record from the current snapshot.
    pub fn get(&self, key: &RouteKey) -> Option<RouteData> {
        if self.mutations.is_indeterminate() {
            return None;
        }
        self.snapshot.load().routes.by_key.get(key).cloned()
    }

    /// Return a complete clone of the current logical route map.
    pub fn all(&self) -> BTreeMap<RouteKey, RouteData> {
        if self.mutations.is_indeterminate() {
            return BTreeMap::new();
        }
        self.snapshot.load().routes.by_key.clone()
    }

    /// Resolve a runtime request path against the current immutable snapshot.
    pub fn resolve(&self, request_path: &str) -> Option<RouteMatch> {
        if self.mutations.is_indeterminate() {
            return None;
        }
        self.snapshot.load().resolve(request_path)
    }

    /// Publish an observed activity timestamp before persistence is attempted.
    ///
    /// This is intentionally separate from [`Self::update_activity`], whose
    /// persistence-first contract is used by management mutations. The proxy
    /// data plane uses this method so a completed request is immediately
    /// observable even when best-effort activity persistence later fails.
    pub fn observe_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> bool {
        if self.mutations.is_indeterminate() {
            return false;
        }
        loop {
            let current = self.snapshot.load_full();
            let mut routes = current.routes.clone();
            let Some(existing) = routes.by_key.get(key) else {
                return false;
            };
            if existing.last_activity >= at {
                return false;
            }
            routes.update_activity(key, at);
            let next = Arc::new(RouteSnapshot::from_ordered_routes(routes));
            let previous = self.snapshot.compare_and_swap(&current, next);
            if Arc::ptr_eq(&current, &previous) {
                return true;
            }
        }
    }

    /// Persist a previously observed proxy activity timestamp.
    pub async fn persist_observed_activity(
        self: &Arc<Self>,
        key: &RouteKey,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let registry = Arc::clone(self);
        let key = key.clone();
        self.run_mutation(MutationOperation::UpdateActivity, async move {
            let Some(current) = registry.get(&key) else {
                return Ok(());
            };
            registry
                .store
                .update_activity(&key, current.last_activity.max(at))
                .await
        })
        .await
    }

    /// Persist a route replacement and publish it atomically on success.
    pub async fn put(self: &Arc<Self>, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        let registry = Arc::clone(self);
        self.run_mutation(MutationOperation::Put, async move {
            registry.put_owned(key, data).await
        })
        .await
    }

    async fn put_owned(self: &Arc<Self>, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        let activity_floor = self.activity_floor(key.clone());
        let data = self
            .store
            .put_preserving_activity(key.clone(), data, activity_floor)
            .await?;
        self.merge_and_publish(|routes| {
            routes.replace_preserving_activity(key.clone(), data.clone())
        });
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
        self: &Arc<Self>,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
    ) -> Result<(), StoreError> {
        let activity_floor = self.activity_floor(key.clone());
        let data = self
            .store
            .add(key.clone(), target, extra, activity_floor)
            .await?;
        self.merge_and_publish(|routes| {
            routes.replace_preserving_activity(key.clone(), data.clone())
        });
        Ok(())
    }

    fn activity_floor(self: &Arc<Self>, key: RouteKey) -> ActivityFloor {
        let registry = Arc::downgrade(self);
        ActivityFloor::dynamic(move || {
            registry
                .upgrade()
                .and_then(|registry| registry.get(&key))
                .map(|route| route.last_activity)
        })
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
        self.store.update_activity(&key, at).await?;
        self.merge_and_publish(|routes| routes.update_activity(&key, at));
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
        let deleted = self.store.delete(&key).await?;
        self.merge_and_publish(|routes| routes.remove(&key));
        Ok(deleted)
    }

    fn merge_and_publish(&self, mut apply: impl FnMut(&mut OrderedRoutes)) {
        loop {
            let current = self.snapshot.load_full();
            let mut routes = current.routes.clone();
            apply(&mut routes);
            let next = Arc::new(RouteSnapshot::from_ordered_routes(routes));
            let previous = self.snapshot.compare_and_swap(&current, next);
            if Arc::ptr_eq(&current, &previous) {
                return;
            }
        }
    }

    /// Seal mutation admission and wait at most `timeout` for accepted work.
    ///
    /// Sealing is terminal: this registry rejects every later mutation with
    /// [`MUTATION_ADMISSION_SEALED_ERROR`]. Detached diagnostics are returned
    /// once and consumed by this call. A timeout does not cancel pending work;
    /// a later drain remains sealed and can finish observing it.
    pub async fn drain_mutations(&self, timeout: Duration) -> MutationDrainOutcome {
        self.mutations.seal_and_drain(timeout).await
    }

    /// Observe mutation admission without sealing or consuming diagnostics.
    pub fn mutation_status(&self) -> MutationStatus {
        self.mutations.status()
    }

    /// Report whether this registry may safely serve its cached snapshot.
    ///
    /// Indeterminate is terminal for this instance. Recovery requires loading
    /// authoritative backend state into a new registry/process.
    pub fn consistency_status(&self) -> ConsistencyStatus {
        if self.mutations.is_indeterminate() {
            ConsistencyStatus::Indeterminate
        } else {
            ConsistencyStatus::Consistent
        }
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
        let active = self.mutations.begin(operation)?;
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                active.finish(None, None);
                return Err(StoreError::message(format!(
                    "route {} mutation could not start: Tokio runtime unavailable",
                    operation.name()
                )));
            }
        };
        let (response, receiver) = oneshot::channel();
        let mutation_lock = Arc::clone(&self.mutation);
        let tracker = Arc::clone(&self.mutations);
        let spawned = catch_unwind(AssertUnwindSafe(|| {
            runtime.spawn(async move {
                let _mutation_guard = mutation_lock.lock().await;
                let supervised = match tracker.reject_if_terminally_failed() {
                    Ok(()) => CatchUnwindFuture::new(mutation, Arc::clone(&tracker)).await,
                    Err(error) => Ok(Err(error)),
                };
                match supervised {
                    Ok(result) => {
                        if matches!(result, Err(StoreError::Indeterminate { .. })) {
                            tracker.indeterminate_seal();
                        }
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
            })
        }));
        if let Ok(handle) = spawned {
            drop(handle);
        }

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
