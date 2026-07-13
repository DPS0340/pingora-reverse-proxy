//! Redis-backed route persistence using one versioned hash.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use redis::aio::MultiplexedConnection;
use redis::AsyncCommands;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::route::{RouteData, RouteKey};

use super::{ActivityFloor, Store, StoreError};

pub const DEFAULT_REDIS_ROUTE_KEY: &str = "pingora-reverse-proxy:routes:v1";
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

/// Connection and key-space settings for [`RedisStore`].
#[derive(Clone, PartialEq, Eq)]
pub struct RedisStoreConfig {
    url: String,
    key: String,
    operation_timeout: Duration,
}

impl std::fmt::Debug for RedisStoreConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedisStoreConfig")
            .field("url", &"<redacted>")
            .field("key", &self.key)
            .field("operation_timeout", &self.operation_timeout)
            .finish()
    }
}

impl RedisStoreConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            key: DEFAULT_REDIS_ROUTE_KEY.to_owned(),
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
        }
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    pub fn with_operation_timeout(mut self, operation_timeout: Duration) -> Self {
        self.operation_timeout = operation_timeout;
        self
    }
}

/// Persistent route store backed by a single Redis hash.
pub struct RedisStore {
    client: redis::Client,
    connection: Mutex<Option<MultiplexedConnection>>,
    key: String,
    operation_timeout: Duration,
}

impl std::fmt::Debug for RedisStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedisStore")
            .field("key", &self.key)
            .field("operation_timeout", &self.operation_timeout)
            .finish_non_exhaustive()
    }
}

impl RedisStore {
    pub async fn connect(config: RedisStoreConfig) -> Result<Self, StoreError> {
        let client = redis::Client::open(config.url.as_str()).map_err(|_| StoreError::Backend {
            operation: "connect",
        })?;
        let connection_config = redis::AsyncConnectionConfig::new()
            .set_connection_timeout(config.operation_timeout)
            .set_response_timeout(config.operation_timeout);
        let connection = client
            .get_multiplexed_async_connection_with_config(&connection_config)
            .await
            .map_err(|_| StoreError::Backend {
                operation: "connect",
            })?;

        Ok(Self {
            client,
            connection: Mutex::new(Some(connection)),
            key: config.key,
            operation_timeout: config.operation_timeout,
        })
    }

    fn backend_error(operation: &'static str) -> impl FnOnce(redis::RedisError) -> StoreError {
        move |_| StoreError::Backend { operation }
    }

    fn encode(operation: &'static str, data: &RouteData) -> Result<String, StoreError> {
        let mut encoded = serde_json::to_value(data).map_err(|_| StoreError::CorruptData {
            operation,
            key: "<outgoing>".to_owned(),
        })?;
        let object = encoded
            .as_object_mut()
            .ok_or_else(|| StoreError::CorruptData {
                operation,
                key: "<outgoing>".to_owned(),
            })?;
        object.insert(
            "last_activity".to_owned(),
            Value::String(
                data.last_activity
                    .to_rfc3339_opts(SecondsFormat::Nanos, true),
            ),
        );
        serde_json::to_string(&encoded).map_err(|_| StoreError::CorruptData {
            operation,
            key: "<outgoing>".to_owned(),
        })
    }

    fn decode(operation: &'static str, key: &str, encoded: &str) -> Result<RouteData, StoreError> {
        serde_json::from_str(encoded).map_err(|_| StoreError::CorruptData {
            operation,
            key: key.to_owned(),
        })
    }

    fn decode_key(field: &str) -> Result<RouteKey, StoreError> {
        if !field.starts_with('/') {
            return Err(StoreError::CorruptData {
                operation: "snapshot",
                key: field.to_owned(),
            });
        }
        // RouteKey performs CHP's one-character trailing-slash cleanup on
        // construction. Add that character back so a trusted persisted field
        // is reconstructed exactly instead of normalized a second time.
        let parse_input = if field.len() > 1 && field.ends_with('/') {
            format!("{field}/")
        } else {
            field.to_owned()
        };
        Ok(RouteKey::parse(&parse_input).expect("route-key normalization is infallible"))
    }

    fn connection_config(&self) -> redis::AsyncConnectionConfig {
        redis::AsyncConnectionConfig::new()
            .set_connection_timeout(self.operation_timeout)
            .set_response_timeout(self.operation_timeout)
    }

    async fn connect_new(
        &self,
        operation: &'static str,
    ) -> Result<MultiplexedConnection, StoreError> {
        tokio::time::timeout(
            self.operation_timeout,
            self.client
                .get_multiplexed_async_connection_with_config(&self.connection_config()),
        )
        .await
        .map_err(|_| StoreError::Backend { operation })?
        .map_err(Self::backend_error(operation))
    }

    async fn take_connection(
        &self,
        operation: &'static str,
    ) -> Result<MultiplexedConnection, StoreError> {
        if let Some(connection) = self.connection.lock().await.take() {
            Ok(connection)
        } else {
            self.connect_new(operation).await
        }
    }

    async fn return_connection(&self, mut connection: MultiplexedConnection) {
        connection.set_response_timeout(self.operation_timeout);
        *self.connection.lock().await = Some(connection);
    }

    fn use_remaining_timeout(
        &self,
        operation: &'static str,
        deadline: tokio::time::Instant,
        connection: &mut MultiplexedConnection,
    ) -> Result<(), StoreError> {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or(StoreError::Backend { operation })?;
        connection.set_response_timeout(remaining);
        Ok(())
    }

    async fn reconcile(
        &self,
        operation: &'static str,
        key: &RouteKey,
    ) -> Result<Option<String>, StoreError> {
        let mut connection = self.connect_new(operation).await?;
        let current = tokio::time::timeout(
            self.operation_timeout,
            connection.hget(&self.key, key.as_str()),
        )
        .await
        .map_err(|_| StoreError::Backend { operation })?
        .map_err(Self::backend_error(operation))?;
        self.return_connection(connection).await;
        Ok(current)
    }

    async fn atomic_replace(
        &self,
        operation: &'static str,
        key: &RouteKey,
        mut data: RouteData,
        activity_floor: &ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut connection = self.take_connection(operation).await?;
        let deadline = tokio::time::Instant::now() + self.operation_timeout;
        loop {
            self.use_remaining_timeout(operation, deadline, &mut connection)?;
            if redis::cmd("WATCH")
                .arg(&self.key)
                .query_async::<()>(&mut connection)
                .await
                .is_err()
            {
                return Err(StoreError::Backend { operation });
            }

            if let Some(floor) = activity_floor.current() {
                data.last_activity = data.last_activity.max(floor);
            }
            let encoded = Self::encode(operation, &data)?;
            let committed_data = Self::decode(operation, key.as_str(), &encoded)?;
            self.use_remaining_timeout(operation, deadline, &mut connection)?;
            let transaction = redis::pipe()
                .atomic()
                .cmd("HSET")
                .arg(&self.key)
                .arg(key.as_str())
                .arg(&encoded)
                .ignore()
                .query_async::<Option<()>>(&mut connection)
                .await;
            match transaction {
                Ok(Some(())) => {
                    self.return_connection(connection).await;
                    return Ok(committed_data);
                }
                Ok(None) => continue,
                Err(_) => {
                    drop(connection);
                    return if self.reconcile(operation, key).await? == Some(encoded) {
                        Ok(committed_data)
                    } else {
                        Err(StoreError::Backend { operation })
                    };
                }
            }
        }
    }
}

#[async_trait]
impl Store for RedisStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        let mut connection = self.take_connection("snapshot").await?;
        let records: Vec<(String, String)> = connection
            .hgetall(&self.key)
            .await
            .map_err(Self::backend_error("snapshot"))?;
        let mut snapshot = BTreeMap::new();
        for (field, encoded) in records {
            let key = Self::decode_key(&field)?;
            let data = Self::decode("snapshot", key.as_str(), &encoded)?;
            snapshot.insert(key, data);
        }
        self.return_connection(connection).await;
        Ok(snapshot)
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.atomic_replace(
            "add",
            &key,
            RouteData {
                target,
                last_activity: Utc::now(),
                extra,
            },
            &activity_floor,
        )
        .await
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        let encoded = Self::encode("put", &data)?;
        let mut connection = self.take_connection("put").await?;
        match connection
            .hset::<_, _, _, ()>(&self.key, key.as_str(), &encoded)
            .await
        {
            Ok(()) => {
                self.return_connection(connection).await;
                Ok(())
            }
            Err(_) => {
                drop(connection);
                if self.reconcile("put", &key).await? == Some(encoded) {
                    Ok(())
                } else {
                    Err(StoreError::Backend { operation: "put" })
                }
            }
        }
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.atomic_replace("put", &key, data, &activity_floor)
            .await
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        let mut connection = self.take_connection("update_activity").await?;
        let deadline = tokio::time::Instant::now() + self.operation_timeout;
        loop {
            self.use_remaining_timeout("update_activity", deadline, &mut connection)?;
            if redis::cmd("WATCH")
                .arg(&self.key)
                .query_async::<()>(&mut connection)
                .await
                .is_err()
            {
                return Err(StoreError::Backend {
                    operation: "update_activity",
                });
            }
            self.use_remaining_timeout("update_activity", deadline, &mut connection)?;
            let current: Option<String> = match connection.hget(&self.key, key.as_str()).await {
                Ok(current) => current,
                Err(_) => {
                    return Err(StoreError::Backend {
                        operation: "update_activity",
                    });
                }
            };
            let Some(current) = current else {
                self.use_remaining_timeout("update_activity", deadline, &mut connection)?;
                if redis::cmd("UNWATCH")
                    .query_async::<()>(&mut connection)
                    .await
                    .is_err()
                {
                    return Err(StoreError::Backend {
                        operation: "update_activity",
                    });
                }
                self.return_connection(connection).await;
                return Ok(());
            };
            let mut data = Self::decode("update_activity", key.as_str(), &current)?;
            data.last_activity = at;
            let encoded = Self::encode("update_activity", &data)?;
            self.use_remaining_timeout("update_activity", deadline, &mut connection)?;
            let transaction = redis::pipe()
                .atomic()
                .cmd("HSET")
                .arg(&self.key)
                .arg(key.as_str())
                .arg(&encoded)
                .ignore()
                .query_async::<Option<()>>(&mut connection)
                .await;
            match transaction {
                Ok(Some(())) => {
                    self.return_connection(connection).await;
                    return Ok(());
                }
                Ok(None) => continue,
                Err(_) => {
                    drop(connection);
                    return if self.reconcile("update_activity", key).await? == Some(encoded) {
                        Ok(())
                    } else {
                        Err(StoreError::Backend {
                            operation: "update_activity",
                        })
                    };
                }
            }
        }
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        let mut connection = self.take_connection("delete").await?;
        let deadline = tokio::time::Instant::now() + self.operation_timeout;
        loop {
            self.use_remaining_timeout("delete", deadline, &mut connection)?;
            if redis::cmd("WATCH")
                .arg(&self.key)
                .query_async::<()>(&mut connection)
                .await
                .is_err()
            {
                return Err(StoreError::Backend {
                    operation: "delete",
                });
            }
            self.use_remaining_timeout("delete", deadline, &mut connection)?;
            let current: Option<String> = match connection.hget(&self.key, key.as_str()).await {
                Ok(current) => current,
                Err(_) => {
                    return Err(StoreError::Backend {
                        operation: "delete",
                    });
                }
            };
            let Some(encoded) = current else {
                self.use_remaining_timeout("delete", deadline, &mut connection)?;
                if redis::cmd("UNWATCH")
                    .query_async::<()>(&mut connection)
                    .await
                    .is_err()
                {
                    return Err(StoreError::Backend {
                        operation: "delete",
                    });
                }
                self.return_connection(connection).await;
                return Ok(None);
            };
            let prior = Self::decode("delete", key.as_str(), &encoded)?;
            self.use_remaining_timeout("delete", deadline, &mut connection)?;
            let transaction = redis::pipe()
                .atomic()
                .cmd("HDEL")
                .arg(&self.key)
                .arg(key.as_str())
                .ignore()
                .query_async::<Option<()>>(&mut connection)
                .await;
            match transaction {
                Ok(Some(())) => {
                    self.return_connection(connection).await;
                    return Ok(Some(prior));
                }
                Ok(None) => continue,
                Err(_) => {
                    drop(connection);
                    return if self.reconcile("delete", key).await?.is_none() {
                        Ok(Some(prior))
                    } else {
                        Err(StoreError::Backend {
                            operation: "delete",
                        })
                    };
                }
            }
        }
    }
}
