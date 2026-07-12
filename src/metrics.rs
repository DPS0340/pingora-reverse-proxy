//! Counters shared by the management API and later Prometheus rendering.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process metrics used by request handlers.
#[derive(Debug, Default)]
pub struct Metrics {
    api_route_get: AtomicU64,
    api_route_add: AtomicU64,
    api_route_delete: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
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
}
