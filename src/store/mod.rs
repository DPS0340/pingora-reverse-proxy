//! Backend-neutral route persistence.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::route::{RouteData, RouteKey};

pub mod memory;
pub mod redis;
pub mod sidecar;

/// A monotonic activity source sampled inside an atomic route replacement.
#[derive(Clone)]
pub struct ActivityFloor {
    current: Arc<dyn Fn() -> Option<DateTime<Utc>> + Send + Sync>,
}

impl ActivityFloor {
    /// Construct a stable floor for direct backend operations and tests.
    pub fn fixed(at: Option<DateTime<Utc>>) -> Self {
        Self {
            current: Arc::new(move || at),
        }
    }

    pub(crate) fn dynamic(
        current: impl Fn() -> Option<DateTime<Utc>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            current: Arc::new(current),
        }
    }

    /// Read the newest activity timestamp at the backend's commit boundary.
    pub fn current(&self) -> Option<DateTime<Utc>> {
        (self.current)()
    }
}

/// Error returned by a route persistence backend.
#[derive(Clone, Debug, Error)]
pub enum StoreError {
    #[error("{0}")]
    Message(String),
    #[error("Route store {operation} operation failed")]
    Backend { operation: &'static str },
    /// A remote mutation was dispatched, but its reply was lost.
    ///
    /// The backend may or may not contain the mutation. Callers must fail stop
    /// and recover by loading authoritative state into a new registry/process.
    #[error("Route store {operation} operation outcome is indeterminate")]
    Indeterminate { operation: &'static str },
    #[error("Route store {operation} found a corrupt route record for {key:?}")]
    CorruptData {
        operation: &'static str,
        key: String,
    },
}

impl StoreError {
    /// Construct a backend error with a human-readable message.
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Persistent route operations required by the registry.
///
/// Ordinary errors guarantee that backend state was not changed by the failed
/// operation. [`StoreError::Indeterminate`] instead means a dispatched remote
/// mutation has an unknown outcome and requires fail-stop plus authoritative
/// reload; later reads must never be used to infer which writer produced state.
#[async_trait]
pub trait Store: Send + Sync {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError>;
    /// Atomically add or replace a route and assign its final activity stamp.
    ///
    /// The target, metadata, and final activity stamp must be committed as one
    /// atomic backend mutation, and success must return that exact record.
    /// Remote implementations may yield between their commit and returning the
    /// record. Caller-cancellation safety across that interval is owned by
    /// `RouteRegistry`, which executes the complete logical mutation in its own
    /// task. Backends should apply finite operation timeouts so registry-owned
    /// tasks do not remain pending indefinitely.
    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError>;
    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError>;
    /// Atomically replace a complete route without a corrective second write.
    ///
    /// Every backend must sample `activity_floor` inside the same lock or
    /// transaction as the replacement, immediately before its atomic commit.
    /// An ordinary error must leave backend state unchanged. An indeterminate
    /// error follows the fail-stop/reload contract documented on [`Store`].
    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError>;
    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError>;
    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError>;
}
