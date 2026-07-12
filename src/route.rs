//! Normalized route keys and their persisted data.

use std::convert::Infallible;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Error returned while parsing a route key.
///
/// CHP route-key cleanup is infallible, so this type currently has no values.
pub type RouteError = Infallible;

/// A route key cleaned with CHP's single-leading/single-trailing-slash rules.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RouteKey(String);

impl RouteKey {
    /// Normalize a CHP route key while preserving the root route.
    pub fn parse(input: &str) -> Result<Self, RouteError> {
        Ok(Self::normalize(input))
    }

    fn normalize(input: &str) -> Self {
        let mut normalized = input.to_owned();
        if normalized.is_empty() || !normalized.starts_with('/') {
            normalized.insert(0, '/');
        }
        if normalized.len() > 1 && normalized.ends_with('/') {
            normalized.pop();
        }

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
    pub target: String,
    #[serde(serialize_with = "serialize_chp_date")]
    pub last_activity: DateTime<Utc>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn serialize_chp_date<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_rfc3339_opts(SecondsFormat::Millis, true))
}
