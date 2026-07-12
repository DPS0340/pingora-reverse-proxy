//! Normalized route keys and their persisted data.

use std::convert::Infallible;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use url::Url;

/// Error returned while parsing a route key.
///
/// CHP route-key cleanup is infallible, so this type currently has no values.
pub type RouteError = Infallible;

/// A route key normalized to one leading slash and no trailing slash except at root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RouteKey(String);

impl RouteKey {
    /// Normalize a CHP route key while preserving the root route.
    pub fn parse(input: &str) -> Result<Self, RouteError> {
        Ok(Self::normalize(input))
    }

    fn normalize(input: &str) -> Self {
        let path = input.trim_matches('/');
        let normalized = if path.is_empty() {
            "/".to_owned()
        } else {
            format!("/{path}")
        };

        Self(normalized)
    }

    /// Return the normalized route key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RouteKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::normalize(&raw))
    }
}

/// Persisted data for a route, including CHP-compatible extension fields.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RouteData {
    pub target: Url,
    pub last_activity: DateTime<Utc>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
