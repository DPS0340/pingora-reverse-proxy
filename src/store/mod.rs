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
    /// Backends perform any fallible or asynchronous persistence work before
    /// the stamp-and-commit step. Returning `Err`, or cancellation while the
    /// future is pending, must leave the stored route unchanged. After commit,
    /// implementations must return the exact committed record without another
    /// yield point. This boundary maps to a Redis Lua script or atomic sidecar
    /// operation without exposing a partially updated route to the registry.
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
