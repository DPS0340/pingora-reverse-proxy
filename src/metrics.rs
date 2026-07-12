//! Testable counters shared by the management API and later Prometheus rendering.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Point-in-time counter values, independent of the Task 11 rendering format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricsSnapshot {
    pub requests_api: BTreeMap<u16, u64>,
    pub api_route_get: u64,
    pub api_route_add: u64,
    pub api_route_delete: u64,
}

/// Process metrics used by request handlers.
#[derive(Debug, Default)]
pub struct Metrics {
    requests_api: Mutex<BTreeMap<u16, u64>>,
    api_route_get: AtomicU64,
    api_route_add: AtomicU64,
    api_route_delete: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_api_request(&self, status: u16) {
        let mut counts = self
            .requests_api
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counts.entry(status).or_default() += 1;
    }

    pub(crate) fn record_api_route_get(&self) {
        self.api_route_get.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_api_route_add(&self) {
        self.api_route_add.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_api_route_delete(&self) {
        self.api_route_delete.fetch_add(1, Ordering::Relaxed);
    }

    /// Return stable values for contract tests and future metric rendering.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests_api: self
                .requests_api
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            api_route_get: self.api_route_get.load(Ordering::Relaxed),
            api_route_add: self.api_route_add.load(Ordering::Relaxed),
            api_route_delete: self.api_route_delete.load(Ordering::Relaxed),
        }
    }
}
