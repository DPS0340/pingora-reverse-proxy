//! Bounded, coalesced persistence for proxy activity observations.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, Notify};

use crate::route::RouteKey;
use crate::route_table::RouteRegistry;

struct ActivityState {
    pending: Mutex<BTreeMap<RouteKey, DateTime<Utc>>>,
    in_flight: AtomicUsize,
    persistence_errors: AtomicU64,
    idle: Notify,
}

/// Non-blocking activity recorder backed by one bounded wake-up channel.
///
/// Timestamps are published to the registry before this type returns. Pending
/// persistence is stored once per route, so a burst coalesces to the newest
/// timestamp even when the bounded channel is full.
#[derive(Clone)]
pub struct ActivityWriter {
    registry: Arc<RouteRegistry>,
    state: Arc<ActivityState>,
    wake: mpsc::Sender<()>,
}

impl ActivityWriter {
    /// Start the persistence worker on the current Tokio runtime.
    pub fn start(registry: Arc<RouteRegistry>, channel_capacity: usize) -> Self {
        let state = Arc::new(ActivityState {
            pending: Mutex::new(BTreeMap::new()),
            in_flight: AtomicUsize::new(0),
            persistence_errors: AtomicU64::new(0),
            idle: Notify::new(),
        });
        let (wake, mut receiver) = mpsc::channel(channel_capacity.max(1));
        let worker_registry = Arc::clone(&registry);
        let worker_state = Arc::clone(&state);
        tokio::spawn(async move {
            while receiver.recv().await.is_some() {
                loop {
                    let pending = {
                        let mut guard = worker_state
                            .pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let pending = std::mem::take(&mut *guard);
                        worker_state
                            .in_flight
                            .fetch_add(pending.len(), Ordering::AcqRel);
                        pending
                    };
                    if pending.is_empty() {
                        worker_state.idle.notify_waiters();
                        break;
                    }
                    for (key, pending_at) in pending {
                        let at = worker_registry
                            .get(&key)
                            .map_or(pending_at, |route| route.last_activity.max(pending_at));
                        if worker_registry
                            .persist_observed_activity(&key, at)
                            .await
                            .is_err()
                        {
                            worker_state
                                .persistence_errors
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        worker_state.in_flight.fetch_sub(1, Ordering::AcqRel);
                    }
                    worker_state.idle.notify_waiters();
                }
            }
        });
        Self {
            registry,
            state,
            wake,
        }
    }

    /// Observe activity using the current UTC timestamp.
    pub fn record(&self, key: &RouteKey) {
        self.record_at(key, Utc::now());
    }

    /// Observe activity at a caller-supplied timestamp.
    pub fn record_at(&self, key: &RouteKey, at: DateTime<Utc>) {
        if !self.registry.observe_activity(key, at) {
            return;
        }
        let mut pending = self
            .state
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending
            .entry(key.clone())
            .and_modify(|existing| *existing = (*existing).max(at))
            .or_insert(at);
        drop(pending);
        if matches!(
            self.wake.try_send(()),
            Err(mpsc::error::TrySendError::Closed(_))
        ) {
            self.state
                .persistence_errors
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Number of failed persistence attempts observed by the worker.
    pub fn persistence_errors(&self) -> u64 {
        self.state.persistence_errors.load(Ordering::Relaxed)
    }

    /// Wait until all activity accepted before this observation has drained.
    pub async fn flush(&self) {
        loop {
            let notified = self.state.idle.notified();
            let pending_is_empty = self
                .state
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty();
            if pending_is_empty && self.state.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}
