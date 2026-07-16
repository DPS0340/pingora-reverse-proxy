//! In-memory implementation of the route store contract.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::RwLock;

use crate::route::{RouteData, RouteKey};

use super::{differential_fixed_now, ActivityFloor, Store, StoreError};

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

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut routes = self.routes.write().await;
        let now = differential_fixed_now().unwrap_or_else(Utc::now);
        let data = RouteData {
            target,
            last_activity: activity_floor.current().map_or(now, |floor| now.max(floor)),
            extra,
        };
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
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
