//! Bounded, coalesced persistence for proxy activity observations.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, Notify};

use crate::metrics::Metrics;
use crate::route::RouteKey;
use crate::route_table::RouteRegistry;
use crate::store::differential_fixed_now;

struct ActivityState {
    pending: Mutex<PendingActivities>,
    persistence_errors: AtomicU64,
    dropped_observations: AtomicU64,
    pending_capacity: usize,
    metrics: Arc<Metrics>,
    idle: Notify,
}

#[derive(Default)]
struct PendingActivities {
    by_key: BTreeMap<RouteKey, PendingActivity>,
    ready: VecDeque<RouteKey>,
    accepted_through: u64,
}

#[derive(Clone, Copy)]
struct PendingActivity {
    latest: DateTime<Utc>,
    pending_oldest_sequence: Option<u64>,
    in_flight_oldest_sequence: Option<u64>,
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
    pub fn start(registry: Arc<RouteRegistry>, pending_capacity: usize) -> Self {
        Self::start_with_metrics(registry, pending_capacity, Arc::new(Metrics::new()))
    }

    /// Start the worker and report persistence failures and admission drops to process metrics.
    pub fn start_with_metrics(
        registry: Arc<RouteRegistry>,
        pending_capacity: usize,
        metrics: Arc<Metrics>,
    ) -> Self {
        let pending_capacity = pending_capacity.max(1);
        let state = Arc::new(ActivityState {
            pending: Mutex::new(PendingActivities::default()),
            persistence_errors: AtomicU64::new(0),
            dropped_observations: AtomicU64::new(0),
            pending_capacity,
            metrics,
            idle: Notify::new(),
        });
        let (wake, mut receiver) = mpsc::channel(1);
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
                        guard.ready.pop_front().map(|key| {
                            let pending = guard
                                .by_key
                                .get_mut(&key)
                                .expect("ready activity key must remain resident");
                            let oldest_sequence = pending
                                .pending_oldest_sequence
                                .take()
                                .expect("ready activity must have a pending acceptance");
                            pending.in_flight_oldest_sequence = Some(oldest_sequence);
                            (key, pending.latest, oldest_sequence)
                        })
                    };
                    let Some((key, pending_at, pending_sequence)) = pending else {
                        worker_state.idle.notify_waiters();
                        break;
                    };
                    if worker_registry
                        .persist_observed_activity(&key, pending_at)
                        .await
                        .is_err()
                    {
                        let previous = worker_state
                            .persistence_errors
                            .fetch_add(1, Ordering::Relaxed);
                        worker_state.metrics.record_activity_persistence_failure();
                        if (previous + 1).is_power_of_two() {
                            tracing::warn!(failures = previous + 1, "activity persistence failed");
                        }
                    }
                    let mut guard = worker_state
                        .pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let has_later_activity = guard.by_key.get_mut(&key).is_some_and(|pending| {
                        debug_assert_eq!(pending.in_flight_oldest_sequence, Some(pending_sequence));
                        pending.in_flight_oldest_sequence = None;
                        pending.pending_oldest_sequence.is_some()
                    });
                    if !has_later_activity {
                        guard.by_key.remove(&key);
                    } else {
                        guard.ready.push_back(key);
                    }
                    drop(guard);
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
        if differential_fixed_now().is_some() {
            // CHP still times an update when its frozen Date equals the stored
            // value; mirror that no-op accounting without scheduling a write.
            let started = Instant::now();
            if self.registry.get(key).is_some() {
                self.state
                    .metrics
                    .record_last_activity_update(started.elapsed());
            }
            return;
        }
        self.record_at(key, Utc::now());
    }

    /// Observe activity at a caller-supplied timestamp.
    pub fn record_at(&self, key: &RouteKey, at: DateTime<Utc>) {
        let started = Instant::now();
        if !self.registry.observe_activity(key, at) {
            return;
        }
        self.state
            .metrics
            .record_last_activity_update(started.elapsed());
        let mut pending = self
            .state
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let accepted =
            pending.by_key.contains_key(key) || pending.by_key.len() < self.state.pending_capacity;
        if !accepted {
            let previous = self
                .state
                .dropped_observations
                .fetch_add(1, Ordering::Relaxed);
            self.state.metrics.record_activity_drop();
            if (previous + 1).is_power_of_two() {
                tracing::warn!(
                    drops = previous + 1,
                    "activity persistence observation dropped: pending capacity reached"
                );
            }
            return;
        }
        pending.accepted_through = pending
            .accepted_through
            .checked_add(1)
            .expect("activity acceptance sequence exhausted");
        let sequence = pending.accepted_through;
        if let Some(existing) = pending.by_key.get_mut(key) {
            existing.latest = existing.latest.max(at);
            existing.pending_oldest_sequence.get_or_insert(sequence);
        } else {
            pending.by_key.insert(
                key.clone(),
                PendingActivity {
                    latest: at,
                    pending_oldest_sequence: Some(sequence),
                    in_flight_oldest_sequence: None,
                },
            );
            pending.ready.push_back(key.clone());
        }
        drop(pending);
        if matches!(
            self.wake.try_send(()),
            Err(mpsc::error::TrySendError::Closed(_))
        ) {
            let previous = self
                .state
                .persistence_errors
                .fetch_add(1, Ordering::Relaxed);
            self.state.metrics.record_activity_persistence_failure();
            if (previous + 1).is_power_of_two() {
                tracing::warn!(
                    failures = previous + 1,
                    "activity persistence worker unavailable"
                );
            }
        }
    }

    /// Number of failed persistence attempts observed by the worker.
    pub fn persistence_errors(&self) -> u64 {
        self.state.persistence_errors.load(Ordering::Relaxed)
    }

    /// Number of observations whose new route key could not enter the bounded pending map.
    pub fn dropped_observations(&self) -> u64 {
        self.state.dropped_observations.load(Ordering::Relaxed)
    }

    /// Number of distinct route keys currently waiting for persistence.
    pub fn pending_routes(&self) -> usize {
        self.state
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_key
            .len()
    }

    /// Wait until all activity accepted before this observation has drained.
    pub async fn flush(&self) {
        let watermark = self
            .state
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .accepted_through;
        loop {
            let notified = self.state.idle.notified();
            let watermark_is_complete = self
                .state
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .by_key
                .values()
                .all(|pending| {
                    pending
                        .in_flight_oldest_sequence
                        .is_none_or(|sequence| sequence > watermark)
                        && pending
                            .pending_oldest_sequence
                            .is_none_or(|sequence| sequence > watermark)
                });
            if watermark_is_complete {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.state.metrics)
    }
}
