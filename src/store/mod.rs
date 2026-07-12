//! Backend-neutral route persistence.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::route::{RouteData, RouteKey};

pub mod memory;

/// Error returned by a route persistence backend.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{0}")]
    Message(String),
}

impl StoreError {
    /// Construct a backend error with a human-readable message.
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Persistent route operations required by the registry.
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
    ) -> Result<RouteData, StoreError>;
    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError>;
    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError>;
    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError>;
}
