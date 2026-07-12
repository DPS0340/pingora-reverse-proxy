//! In-memory implementation of the route store contract.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use crate::route::{RouteData, RouteKey};

use super::{Store, StoreError};

/// Process-local route storage primarily used as the default ephemeral backend.
#[derive(Debug, Default)]
pub struct MemoryStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        if let Some(route) = self.routes.write().await.get_mut(key) {
            route.last_activity = at;
        }
        Ok(())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Ok(self.routes.write().await.remove(key))
    }
}
