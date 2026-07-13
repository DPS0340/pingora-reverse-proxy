use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use pingora_reverse_proxy::route::{RouteData, RouteKey};
use pingora_reverse_proxy::route_table::{
    install_route_mutation_panic_hook_at_startup, ConsistencyStatus, MutationOperation,
    MutationSeal, RouteMatch, RouteRegistry, DETACHED_MUTATION_DIAGNOSTIC_CAPACITY,
    MUTATION_ADMISSION_SEALED_ERROR, MUTATION_INDETERMINATE_SEALED_ERROR,
    MUTATION_PANIC_SEALED_ERROR,
};
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::redis::{RedisStore, RedisStoreConfig, DEFAULT_REDIS_ROUTE_KEY};
use pingora_reverse_proxy::store::sidecar::{SidecarConfig, SidecarStore};
use pingora_reverse_proxy::store::{ActivityFloor, Store, StoreError};
use proptest::prelude::*;
use redis::AsyncCommands;
use serde_json::{json, Map, Value};
use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Barrier, RwLock, Semaphore};

#[path = "support/sidecar.rs"]
mod sidecar_support;

use sidecar_support::{
    chp_timestamp, Fault as SidecarFault, FaultGate, RawMutationAckFixture, SidecarFixture,
    ACTIVITY as SIDECAR_ACTIVITY, DELETE as SIDECAR_DELETE, HEALTH as SIDECAR_HEALTH,
    PUT as SIDECAR_PUT, SNAPSHOT as SIDECAR_SNAPSHOT,
};

fn key(path: &str) -> RouteKey {
    RouteKey::parse(path).unwrap()
}

fn route(target: &str) -> RouteData {
    RouteData {
        target: target.to_owned(),
        last_activity: Utc.timestamp_opt(1, 0).unwrap(),
        extra: Map::from_iter([("owner".to_owned(), json!("jupyterhub"))]),
    }
}

fn memory_store() -> Arc<dyn Store> {
    Arc::new(MemoryStore::new())
}

#[test]
fn indeterminate_store_error_is_typed_and_redacted() {
    let error = StoreError::Indeterminate { operation: "put" };

    assert!(matches!(
        error,
        StoreError::Indeterminate { operation: "put" }
    ));
    assert_eq!(
        error.to_string(),
        "Route store put operation outcome is indeterminate"
    );
}

async fn assert_store_contract(store: Arc<dyn Store>) {
    let route_key = key("//user/alice///");
    let add_started_at = Utc::now();
    let added = store
        .add(
            route_key.clone(),
            "http://127.0.0.1:8999/added".to_owned(),
            Map::from_iter([("owner".to_owned(), json!("added"))]),
            ActivityFloor::fixed(None),
        )
        .await
        .unwrap();
    assert!(added.last_activity >= add_started_at);
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&added),
        "add must return the exact atomically committed record"
    );

    let mut original_candidate = route("http://127.0.0.1:9000/base");
    original_candidate.last_activity = added.last_activity + chrono::Duration::seconds(1);
    let original = store
        .put_preserving_activity(
            route_key.clone(),
            original_candidate,
            ActivityFloor::fixed(None),
        )
        .await
        .unwrap();
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&original)
    );

    let activity_floor = original.last_activity + chrono::Duration::seconds(1);
    let replacement = store
        .put_preserving_activity(
            route_key.clone(),
            route("http://127.0.0.1:9001/replaced"),
            ActivityFloor::fixed(Some(activity_floor)),
        )
        .await
        .unwrap();
    assert_eq!(replacement.last_activity, activity_floor);
    assert_eq!(store.snapshot().await.unwrap().len(), 1);
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&replacement)
    );

    let activity = Utc.timestamp_opt(42, 123).unwrap();
    store.update_activity(&route_key, activity).await.unwrap();
    let updated = store.snapshot().await.unwrap().remove(&route_key).unwrap();
    assert_eq!(updated.target, replacement.target);
    assert_eq!(updated.extra, replacement.extra);
    assert_eq!(updated.last_activity, activity);

    let missing = key("/missing");
    store
        .update_activity(&missing, Utc.timestamp_opt(99, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(store.delete(&missing).await.unwrap(), None);

    assert_eq!(store.delete(&route_key).await.unwrap(), Some(updated));
    assert!(store.snapshot().await.unwrap().is_empty());
}

#[tokio::test]
async fn memory_store_satisfies_backend_neutral_contract() {
    assert_store_contract(memory_store()).await;
}

fn sidecar_config(fixture: &SidecarFixture) -> SidecarConfig {
    SidecarConfig::new(fixture.base_url())
        .with_connect_timeout(Duration::from_millis(100))
        .with_request_timeout(Duration::from_millis(100))
        .with_retry_policy(3, Duration::from_millis(10), Duration::from_millis(40))
}

async fn sidecar_store(fixture: &SidecarFixture) -> SidecarStore {
    SidecarStore::connect(sidecar_config(fixture))
        .await
        .unwrap()
}

#[tokio::test]
async fn sidecar_store_satisfies_backend_neutral_contract() {
    let fixture = SidecarFixture::start().await;
    assert_store_contract(Arc::new(sidecar_store(&fixture).await)).await;
}

#[tokio::test]
async fn sidecar_connect_requires_exact_health_version_and_status() {
    for body in [
        json!({"version": "v2", "status": "ok"}),
        json!({"version": "v1", "status": "starting"}),
        json!({"version": "v1", "status": "ok", "extra": true}),
    ] {
        let fixture = SidecarFixture::start().await;
        fixture.set_health_body(body).await;
        let error = SidecarStore::connect(sidecar_config(&fixture))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::Backend {
                operation: "connect"
            }
        ));
        assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 1);
    }
}

#[tokio::test]
async fn sidecar_connect_rejects_malformed_health_without_retry() {
    let fixture = SidecarFixture::start().await;
    fixture.set_raw_health_body(b"not-json").await;

    let error = SidecarStore::connect(sidecar_config(&fixture))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 1);
}

#[tokio::test]
async fn sidecar_connect_requires_exact_protocol_response_header() {
    let fixture = SidecarFixture::start().await;
    fixture.set_health_protocol("v2").await;

    let error = SidecarStore::connect(sidecar_config(&fixture))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 1);
}

#[tokio::test]
async fn sidecar_401_is_fixed_and_attempted_once() {
    let fixture = SidecarFixture::start_with_token(Some("correct-token")).await;
    let config = sidecar_config(&fixture).with_bearer_token("wrong-token");

    let error = SidecarStore::connect(config).await.unwrap_err();

    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 1);
}

#[tokio::test]
async fn sidecar_optional_bearer_auth_authenticates_every_endpoint() {
    let fixture = SidecarFixture::start_with_token(Some("correct-token")).await;
    let store = SidecarStore::connect(sidecar_config(&fixture).with_bearer_token("correct-token"))
        .await
        .unwrap();

    assert!(store.snapshot().await.unwrap().is_empty());
    let route_key = key("/authenticated");
    store
        .put(route_key.clone(), route("http://authenticated.example"))
        .await
        .unwrap();
    store
        .update_activity(&route_key, Utc.timestamp_opt(17, 0).unwrap())
        .await
        .unwrap();
    assert!(store.delete(&route_key).await.unwrap().is_some());
    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 1);
    assert_eq!(fixture.request_count(SIDECAR_SNAPSHOT).await, 1);
    assert_eq!(fixture.request_count(SIDECAR_PUT).await, 1);
    assert_eq!(fixture.request_count(SIDECAR_ACTIVITY).await, 1);
    assert_eq!(fixture.request_count(SIDECAR_DELETE).await, 1);
}

#[tokio::test]
async fn sidecar_health_5xx_retries_with_deterministic_attempt_count() {
    let fixture = SidecarFixture::start().await;
    fixture
        .push_fault(SIDECAR_HEALTH, SidecarFault::ServerError)
        .await;
    fixture
        .push_fault(SIDECAR_HEALTH, SidecarFault::ServerError)
        .await;

    SidecarStore::connect(sidecar_config(&fixture))
        .await
        .unwrap();

    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 3);
}

#[tokio::test]
async fn sidecar_health_transport_timeout_retries_finitely() {
    let fixture = SidecarFixture::start().await;
    for _ in 0..3 {
        fixture
            .push_fault(SIDECAR_HEALTH, SidecarFault::Timeout)
            .await;
    }
    let config = sidecar_config(&fixture).with_request_timeout(Duration::from_millis(30));
    let started = tokio::time::Instant::now();

    let error = SidecarStore::connect(config).await.unwrap_err();

    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 3);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn sidecar_snapshot_retries_transport_and_5xx_with_bounded_backoff() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    fixture
        .push_fault(SIDECAR_SNAPSHOT, SidecarFault::ServerError)
        .await;
    fixture
        .push_fault(SIDECAR_SNAPSHOT, SidecarFault::Timeout)
        .await;
    let started = tokio::time::Instant::now();

    assert!(store.snapshot().await.unwrap().is_empty());

    assert_eq!(fixture.request_count(SIDECAR_SNAPSHOT).await, 3);
    assert!(started.elapsed() >= Duration::from_millis(45));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn sidecar_snapshot_retries_body_read_failure_with_exact_attempt_count() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    fixture
        .push_fault(SIDECAR_SNAPSHOT, SidecarFault::TruncatedBody)
        .await;

    assert!(store.snapshot().await.unwrap().is_empty());

    assert_eq!(fixture.request_count(SIDECAR_SNAPSHOT).await, 2);
}

#[tokio::test]
async fn sidecar_snapshot_malformed_body_is_corrupt_and_not_retried() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    fixture
        .push_fault(SIDECAR_SNAPSHOT, SidecarFault::MalformedBody)
        .await;

    let error = store.snapshot().await.unwrap_err();

    assert!(matches!(
        error,
        StoreError::CorruptData {
            operation: "snapshot",
            ..
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_SNAPSHOT).await, 1);
}

#[tokio::test]
async fn sidecar_put_retries_only_precommit_5xx_with_identical_payload() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    fixture
        .push_fault(SIDECAR_PUT, SidecarFault::ServerError)
        .await;
    fixture
        .push_fault(SIDECAR_PUT, SidecarFault::ServerError)
        .await;
    let expected = route("https://private-target.example/path");

    store.put(key("/retry"), expected.clone()).await.unwrap();

    assert_eq!(fixture.request_count(SIDECAR_PUT).await, 3);
    let requests = fixture.requests().await;
    let bodies: Vec<_> = requests
        .iter()
        .filter(|request| request.method == http::Method::PUT)
        .map(|request| request.body.as_slice())
        .collect();
    assert_eq!(bodies.len(), 3);
    assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(fixture.routes().await["/retry"], expected);
}

#[tokio::test]
async fn sidecar_mutation_5xx_exhaustion_is_backend_and_never_commits() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    for _ in 0..3 {
        fixture
            .push_fault(SIDECAR_PUT, SidecarFault::ServerError)
            .await;
    }

    let error = store
        .put(key("/never-committed"), route("http://private.example"))
        .await
        .unwrap_err();

    assert!(matches!(error, StoreError::Backend { operation: "put" }));
    assert_eq!(fixture.request_count(SIDECAR_PUT).await, 3);
    assert!(fixture.routes().await.is_empty());
}

#[tokio::test]
async fn sidecar_patch_5xx_retries_with_identical_payload() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    store
        .put(key("/activity"), route("http://activity.example"))
        .await
        .unwrap();
    fixture
        .push_fault(sidecar_support::ACTIVITY, SidecarFault::ServerError)
        .await;
    let at = Utc.timestamp_opt(42, 123).unwrap();

    store.update_activity(&key("/activity"), at).await.unwrap();

    let requests = fixture.requests().await;
    let bodies: Vec<_> = requests
        .iter()
        .filter(|request| request.method == http::Method::PATCH)
        .map(|request| request.body.as_slice())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(fixture.routes().await["/activity"].last_activity, at);
}

#[tokio::test]
async fn sidecar_mutation_4xx_is_not_retried() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    fixture
        .push_fault(SIDECAR_PUT, SidecarFault::ClientError)
        .await;

    let error = store
        .put(key("/fixed-error"), route("http://target.invalid"))
        .await
        .unwrap_err();

    assert!(matches!(error, StoreError::Backend { operation: "put" }));
    assert_eq!(fixture.request_count(SIDECAR_PUT).await, 1);
    assert!(fixture.routes().await.is_empty());
}

#[tokio::test]
async fn sidecar_unknown_metadata_and_chp_timestamp_round_trip_exactly() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    let at = Utc.timestamp_opt(123, 456_000_000).unwrap();
    let expected = RouteData {
        target: "https://target.example/path?private=true".to_owned(),
        last_activity: at,
        extra: Map::from_iter([
            ("nested".to_owned(), json!({"array": [1, "two", false]})),
            ("nullable".to_owned(), json!(null)),
        ]),
    };

    store.put(key("/metadata"), expected.clone()).await.unwrap();

    assert_eq!(store.snapshot().await.unwrap()[&key("/metadata")], expected);
    let request = fixture
        .requests()
        .await
        .into_iter()
        .find(|request| request.method == http::Method::PUT)
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(payload["route"]["last_activity"], chp_timestamp(at));
    assert_eq!(
        payload["route"]["nested"],
        json!({"array": [1, "two", false]})
    );
}

#[tokio::test]
async fn sidecar_put_is_idempotent_and_restart_reconnect_preserves_state() {
    let mut fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    let expected = route("http://persistent.example");
    store
        .put(key("/persistent"), expected.clone())
        .await
        .unwrap();
    store
        .put(key("/persistent"), expected.clone())
        .await
        .unwrap();
    assert_eq!(fixture.routes().await.len(), 1);
    let state_before_restart = fixture.state_identity();

    fixture.restart().await;
    assert_ne!(fixture.state_identity(), state_before_restart);
    assert!(fixture.durable_path().is_some_and(std::path::Path::exists));
    let reconnected = sidecar_store(&fixture).await;

    assert_eq!(
        reconnected.snapshot().await.unwrap()[&key("/persistent")],
        expected
    );
}

#[tokio::test]
async fn sidecar_delete_returns_exact_prior_and_404_none() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    let expected = route("http://delete.example");
    store.put(key("/delete"), expected.clone()).await.unwrap();

    assert_eq!(store.delete(&key("/delete")).await.unwrap(), Some(expected));
    assert_eq!(store.delete(&key("/delete")).await.unwrap(), None);
}

#[tokio::test]
async fn sidecar_malformed_delete_200_is_indeterminate_and_seals_after_commit() {
    let fixture = SidecarFixture::start().await;
    let store = Arc::new(sidecar_store(&fixture).await);
    let registry = RouteRegistry::load(store).await.unwrap();
    registry
        .put(key("/delete"), route("http://secret-target.example"))
        .await
        .unwrap();
    fixture
        .push_fault(SIDECAR_DELETE, SidecarFault::MalformedBody)
        .await;

    let error = registry.delete(&key("/delete")).await.unwrap_err();

    assert!(matches!(
        error,
        StoreError::Indeterminate {
            operation: "delete"
        }
    ));
    assert_eq!(registry.mutation_status().seal, MutationSeal::Indeterminate);
    assert!(registry.get(&key("/delete")).is_none());
    assert!(fixture.routes().await.is_empty());
    assert!(!error.to_string().contains("secret-target"));
}

#[tokio::test]
async fn sidecar_route_key_is_exactly_one_rfc3986_encoded_segment() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    let cases = [
        ("/", "%2F"),
        ("//user/alice///", "%2F%2Fuser%2Falice%2F%2F"),
        ("/with space", "%2Fwith%20space"),
        ("/percent%", "%2Fpercent%25"),
        ("/query?", "%2Fquery%3F"),
        ("/fragment#", "%2Ffragment%23"),
        ("/한글", "%2F%ED%95%9C%EA%B8%80"),
    ];
    for (raw, _) in cases {
        store
            .put(key(raw), route("http://encoding.example"))
            .await
            .unwrap();
    }

    let requests = fixture.requests().await;
    let paths: Vec<_> = requests
        .iter()
        .filter(|request| request.method == http::Method::PUT)
        .map(|request| request.path.as_str())
        .collect();
    let expected: Vec<_> = cases
        .iter()
        .map(|(_, encoded)| format!("/v1/routes/{encoded}"))
        .collect();
    assert_eq!(paths, expected);
    assert!(paths.iter().all(|path| {
        path.strip_prefix("/v1/routes/")
            .is_some_and(|segment| !segment.contains('/'))
    }));
}

#[tokio::test]
async fn sidecar_mutation_reply_loss_is_indeterminate_and_seals_registry() {
    let fixture = SidecarFixture::start().await;
    let store = Arc::new(sidecar_store(&fixture).await);
    let registry = RouteRegistry::load(store).await.unwrap();
    fixture
        .push_fault(SIDECAR_PUT, SidecarFault::LoseReply)
        .await;

    let error = registry
        .put(key("/uncertain"), route("http://secret-target.example"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        StoreError::Indeterminate { operation: "put" }
    ));
    assert_eq!(registry.mutation_status().seal, MutationSeal::Indeterminate);
    assert_eq!(
        registry.consistency_status(),
        ConsistencyStatus::Indeterminate
    );
    assert!(registry.get(&key("/uncertain")).is_none());
    assert_eq!(fixture.routes().await.len(), 1);
    assert!(!error.to_string().contains("secret-target"));
}

#[tokio::test]
async fn sidecar_delete_reply_loss_is_indeterminate_and_never_retried() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    store
        .put(key("/uncertain-delete"), route("http://delete.example"))
        .await
        .unwrap();
    fixture
        .push_fault(SIDECAR_DELETE, SidecarFault::LoseReply)
        .await;

    let error = store.delete(&key("/uncertain-delete")).await.unwrap_err();

    assert!(matches!(
        error,
        StoreError::Indeterminate {
            operation: "delete"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_DELETE).await, 1);
    assert!(fixture.routes().await.is_empty());
}

#[tokio::test]
async fn sidecar_config_and_errors_redact_auth_body_and_ambiguous_base_url() {
    let fixture = SidecarFixture::start_with_token(Some("server-token")).await;
    let config = sidecar_config(&fixture).with_bearer_token("super-secret-token");
    assert!(!format!("{config:?}").contains("super-secret-token"));
    let error = SidecarStore::connect(config).await.unwrap_err();
    let surfaces = format!("{error} {error:?}");
    assert!(!surfaces.contains("super-secret-token"));
    assert!(!surfaces.contains("Redis"));

    for ambiguous in [
        format!("{}nested", fixture.base_url()),
        format!("{}?query=secret", fixture.base_url()),
        format!("{}#fragment", fixture.base_url()),
        fixture
            .base_url()
            .replacen("http://", "http://user:secret@", 1),
    ] {
        let error = SidecarStore::connect(SidecarConfig::new(ambiguous))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::Backend {
                operation: "connect"
            }
        ));
        assert!(!format!("{error} {error:?}").contains("secret"));
    }
}

async fn raw_sidecar_request(
    fixture: &SidecarFixture,
    method: http::Method,
    path: &str,
    protocol: Option<&str>,
    content_type: Option<&str>,
    authorization: Option<&str>,
    body: Option<&str>,
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut request = client.request(
        method,
        format!("{}{}", fixture.base_url(), path.trim_start_matches('/')),
    );
    if let Some(protocol) = protocol {
        request = request.header("x-store-protocol", protocol);
    }
    if let Some(content_type) = content_type {
        request = request.header(reqwest::header::CONTENT_TYPE, content_type);
    }
    if let Some(authorization) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }
    if let Some(body) = body {
        request = request.body(body.to_owned());
    }
    request.send().await.unwrap()
}

fn valid_put_envelope(extra: &str) -> String {
    format!(
        r#"{{"version":"v1","operation":"put_preserving_activity","route":{{"target":"http://fixture.example","last_activity":"1970-01-01T00:00:01.000000000Z"}},"activityFloor":"1970-01-01T00:00:01.000000000Z"{extra}}}"#
    )
}

fn assert_indeterminate_and_sealed(
    error: &StoreError,
    operation: &'static str,
    registry: &RouteRegistry,
) {
    assert!(matches!(
        error,
        StoreError::Indeterminate {
            operation: actual
        } if *actual == operation
    ));
    assert_eq!(registry.mutation_status().seal, MutationSeal::Indeterminate);
    assert_eq!(
        registry.consistency_status(),
        ConsistencyStatus::Indeterminate
    );
    assert!(registry.all().is_empty());
}

#[tokio::test]
async fn sidecar_fixture_rejects_nonconformant_put_envelopes_before_commit() {
    let fixture = SidecarFixture::start().await;
    let valid = valid_put_envelope("");
    let cases = [
        (None, valid.clone()),
        (Some("text/plain"), valid.clone()),
        (Some("application/json; charset=utf-8"), valid.clone()),
        (
            Some("application/json"),
            valid_put_envelope(",\"unknown\":true"),
        ),
        (
            Some("application/json"),
            valid.replace(
                "\"activityFloor\":\"1970-01-01T00:00:01.000000000Z\"",
                "\"activityFloor\":null",
            ),
        ),
        (
            Some("application/json"),
            valid.replace(
                "1970-01-01T00:00:01.000000000Z\"}",
                "1970-01-01T00:00:01Z\"}",
            ),
        ),
    ];

    for (content_type, body) in cases {
        let response = raw_sidecar_request(
            &fixture,
            http::Method::PUT,
            "/v1/routes/%2Fstrict",
            Some("v1"),
            content_type,
            None,
            Some(&body),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("x-store-protocol")
                .and_then(|value| value.to_str().ok()),
            Some("v1")
        );
    }
    for (content_type, body) in [
        (
            None,
            r#"{"version":"v1","lastActivity":"1970-01-01T00:00:01.000000000Z"}"#,
        ),
        (
            Some("application/json; charset=utf-8"),
            r#"{"version":"v1","lastActivity":"1970-01-01T00:00:01.000000000Z"}"#,
        ),
        (
            Some("application/json"),
            r#"{"version":"v1","lastActivity":"1970-01-01T00:00:01.000000000Z","unknown":true}"#,
        ),
    ] {
        let response = raw_sidecar_request(
            &fixture,
            http::Method::PATCH,
            "/v1/routes/%2Fstrict/activity",
            Some("v1"),
            content_type,
            None,
            Some(body),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-store-protocol"], "v1");
    }
    let missing_protocol = raw_sidecar_request(
        &fixture,
        http::Method::PUT,
        "/v1/routes/%2Fstrict",
        None,
        Some("application/json"),
        None,
        Some(&valid),
    )
    .await;
    assert_eq!(missing_protocol.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(missing_protocol.headers()["x-store-protocol"], "v1");
    assert!(fixture.routes().await.is_empty());
}

#[tokio::test]
async fn sidecar_fixture_rejects_duplicate_route_fields_before_commit() {
    let fixture = SidecarFixture::start().await;
    let routes = [
        r#"{"target":"http://first.example","target":"http://second.example","last_activity":"1970-01-01T00:00:01.000000000Z"}"#,
        r#"{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000Z","last_activity":"1970-01-01T00:00:02.000000000Z"}"#,
        r#"{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000Z","owner":"first","owner":"second"}"#,
    ];

    for route in routes {
        let body = format!(
            r#"{{"version":"v1","operation":"put_preserving_activity","route":{route},"activityFloor":"1970-01-01T00:00:01.000000000Z"}}"#
        );
        let response = raw_sidecar_request(
            &fixture,
            http::Method::PUT,
            "/v1/routes/%2Fduplicate",
            Some("v1"),
            Some("application/json"),
            None,
            Some(&body),
        )
        .await;

        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-store-protocol"], "v1");
        assert!(fixture.routes().await.is_empty());
    }
}

#[tokio::test]
async fn sidecar_fixture_rejects_repeated_protocol_request_header() {
    let fixture = SidecarFixture::start().await;

    let response = reqwest::Client::new()
        .get(format!("{}v1/health", fixture.base_url()))
        .header("x-store-protocol", "v1")
        .header("x-store-protocol", "v1")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["x-store-protocol"], "v1");
}

#[tokio::test]
async fn sidecar_fixture_fallbacks_and_auth_are_protocol_conformant() {
    let fixture = SidecarFixture::start_with_token(Some("fixture-secret-token")).await;
    for (method, path, body) in [
        (http::Method::GET, "/v1/health", None),
        (http::Method::GET, "/v1/routes", None),
        (
            http::Method::PUT,
            "/v1/routes/%2Fauth",
            Some(valid_put_envelope("")),
        ),
        (
            http::Method::PATCH,
            "/v1/routes/%2Fauth/activity",
            Some(r#"{"version":"v1","lastActivity":"1970-01-01T00:00:01.000000000Z"}"#.to_owned()),
        ),
        (http::Method::DELETE, "/v1/routes/%2Fauth", None),
    ] {
        let response = raw_sidecar_request(
            &fixture,
            method,
            path,
            Some("v1"),
            Some("application/json"),
            None,
            body.as_deref(),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["x-store-protocol"], "v1");
    }

    for (method, path) in [
        (http::Method::GET, "/missing"),
        (http::Method::POST, "/v1/health"),
    ] {
        let missing_protocol_and_auth =
            raw_sidecar_request(&fixture, method.clone(), path, None, None, None, None).await;
        assert_eq!(
            missing_protocol_and_auth.status(),
            reqwest::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            missing_protocol_and_auth.headers()["x-store-protocol"],
            "v1"
        );

        let missing_auth =
            raw_sidecar_request(&fixture, method.clone(), path, Some("v1"), None, None, None).await;
        assert_eq!(missing_auth.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(missing_auth.headers()["x-store-protocol"], "v1");

        let response = raw_sidecar_request(
            &fixture,
            method,
            path,
            Some("v1"),
            None,
            Some("Bearer fixture-secret-token"),
            None,
        )
        .await;
        assert!(matches!(response.status().as_u16(), 404 | 405));
        assert_eq!(response.headers()["x-store-protocol"], "v1");
    }

    for protocol in [None, Some("v1")] {
        let response = raw_sidecar_request(
            &fixture,
            http::Method::GET,
            "/v1/routes/%FF",
            protocol,
            None,
            protocol.map(|_| "Bearer fixture-secret-token"),
            None,
        )
        .await;
        if protocol.is_none() {
            assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        } else {
            assert!(!response.status().is_success());
        }
        assert_eq!(response.headers()["x-store-protocol"], "v1");
    }
}

#[tokio::test]
async fn sidecar_snapshot_strictly_rejects_duplicates_and_noncanonical_timestamps() {
    let record = r#"{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000Z","owner":"jupyterhub"}"#;
    let cases = [
        format!(r#"{{"/duplicate":{record},"/duplicate":{record}}}"#),
        r#"{"/route":{"target":"http://first.example","target":"http://secret-target.example","last_activity":"1970-01-01T00:00:01.000000000Z"}}"#.to_owned(),
        r#"{"/route":{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000Z","last_activity":"1970-01-01T00:00:02.000000000Z"}}"#.to_owned(),
        r#"{"/route":{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000Z","owner":"first","owner":"secret-metadata"}}"#.to_owned(),
        r#"{"/offset":{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000+00:00"}}"#.to_owned(),
        r#"{"/precision":{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01Z"}}"#.to_owned(),
        r#"{"/type":{"target":42,"last_activity":"1970-01-01T00:00:01.000000000Z"}}"#.to_owned(),
        format!(r#"{{"raw-secret-route-key":{record}}}"#),
    ];

    for body in cases {
        let fixture = SidecarFixture::start().await;
        let store = sidecar_store(&fixture).await;
        fixture.set_raw_snapshot_body(body.as_bytes()).await;

        let error = store.snapshot().await.unwrap_err();

        assert!(matches!(
            error,
            StoreError::CorruptData {
                operation: "snapshot",
                ..
            }
        ));
        let surfaces = format!("{error} {error:?}");
        for secret in [
            "secret-target",
            "secret-metadata",
            "/duplicate",
            "raw-secret-route-key",
        ] {
            assert!(!surfaces.contains(secret));
        }
    }
}

#[tokio::test]
async fn sidecar_snapshot_body_and_header_limits_are_nonretryable_and_redacted() {
    for fault in [
        SidecarFault::OversizedBody,
        SidecarFault::OversizedHeaders,
        SidecarFault::TooManyHeaders,
    ] {
        let fixture = SidecarFixture::start().await;
        let store = sidecar_store(&fixture).await;
        if matches!(fault, SidecarFault::OversizedBody) {
            fixture
                .set_raw_snapshot_body(&vec![b's'; 4 * 1024 * 1024 + 1])
                .await;
        } else {
            fixture.push_fault(SIDECAR_SNAPSHOT, fault.clone()).await;
        }

        let error = store.snapshot().await.unwrap_err();

        assert_eq!(fixture.request_count(SIDECAR_SNAPSHOT).await, 1);
        assert!(!format!("{error} {error:?}").contains("ssssssss"));
    }
}

#[tokio::test]
async fn sidecar_health_oversized_body_is_fixed_and_not_retried() {
    let fixture = SidecarFixture::start().await;
    fixture
        .set_raw_health_body(&vec![b'h'; 4 * 1024 * 1024 + 1])
        .await;

    let error = SidecarStore::connect(sidecar_config(&fixture))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 1);
}

#[tokio::test]
async fn sidecar_health_requires_exact_json_content_type() {
    let fixture = SidecarFixture::start().await;
    fixture.set_health_content_type("text/plain").await;

    let error = SidecarStore::connect(sidecar_config(&fixture))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_HEALTH).await, 1);
}

#[tokio::test]
async fn sidecar_snapshot_requires_exact_json_content_type() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    fixture.set_snapshot_content_type("text/plain").await;

    let error = store.snapshot().await.unwrap_err();

    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "snapshot"
        }
    ));
    assert_eq!(fixture.request_count(SIDECAR_SNAPSHOT).await, 1);
}

#[tokio::test]
async fn sidecar_outgoing_mutation_body_is_rejected_before_dispatch() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    let mut huge = route("http://safe.example");
    huge.extra.insert(
        "private".to_owned(),
        json!("outgoing-secret".repeat(80_000)),
    );

    let error = store.put(key("/too-large"), huge).await.unwrap_err();

    assert!(matches!(error, StoreError::Backend { operation: "put" }));
    assert_eq!(fixture.request_count(SIDECAR_PUT).await, 0);
    assert!(!format!("{error} {error:?}").contains("outgoing-secret"));
}

#[tokio::test]
async fn sidecar_put_resamples_activity_floor_before_each_attempt() {
    let mut fixture = SidecarFixture::start().await;
    let store = Arc::new(sidecar_store(&fixture).await);
    let registry = RouteRegistry::load(store).await.unwrap();
    let route_key = key("/dynamic-floor");
    registry
        .put(route_key.clone(), route("http://original.example"))
        .await
        .unwrap();
    let gate = FaultGate::new();
    fixture
        .push_fault(SIDECAR_PUT, SidecarFault::ServerErrorGate(gate.clone()))
        .await;
    let candidate = RouteData {
        target: "http://replacement.example".to_owned(),
        last_activity: Utc.timestamp_opt(2, 0).unwrap(),
        extra: Map::new(),
    };
    let mutation = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move { registry.put(route_key, candidate).await })
    };
    gate.wait_until_arrived().await;
    let independent = sidecar_store(&fixture).await;
    let mut independently_advanced = route("http://independent-retry.example");
    independently_advanced.last_activity = Utc.timestamp_opt(100, 0).unwrap();
    independent
        .put(route_key.clone(), independently_advanced)
        .await
        .unwrap();
    let observed = Utc.timestamp_opt(50, 123).unwrap();
    assert!(registry.observe_activity(&route_key, observed));
    gate.release();

    mutation.await.unwrap().unwrap();

    let persisted = fixture.routes().await.remove("/dynamic-floor").unwrap();
    let expected = RouteData {
        target: "http://replacement.example".to_owned(),
        last_activity: Utc.timestamp_opt(100, 0).unwrap(),
        extra: Map::new(),
    };
    assert_eq!(persisted, expected);
    assert_eq!(registry.get(&route_key).as_ref(), Some(&expected));
    let requests = fixture.requests().await;
    let bodies: Vec<Value> = requests
        .iter()
        .filter(|request| request.method == http::Method::PUT)
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .filter(|body: &Value| {
            body["route"]["target"] == Value::String("http://replacement.example".to_owned())
        })
        .collect();
    let retry = &bodies;
    assert_eq!(retry.len(), 2);
    assert_eq!(retry[0]["route"], retry[1]["route"]);
    assert!(
        retry[0]["activityFloor"].as_str().unwrap() < retry[1]["activityFloor"].as_str().unwrap()
    );
    assert_eq!(retry[1]["activityFloor"], chp_timestamp(observed));

    fixture.restart().await;
    let reconnected = sidecar_store(&fixture).await;
    assert_eq!(reconnected.snapshot().await.unwrap()[&route_key], expected);
}

#[tokio::test]
async fn sidecar_add_resamples_activity_floor_before_each_attempt() {
    let mut fixture = SidecarFixture::start().await;
    let store = Arc::new(sidecar_store(&fixture).await);
    let registry = RouteRegistry::load(store).await.unwrap();
    let route_key = key("/dynamic-add-floor");
    registry
        .put(route_key.clone(), route("http://original-add.example"))
        .await
        .unwrap();
    let gate = FaultGate::new();
    fixture
        .push_fault(SIDECAR_PUT, SidecarFault::ServerErrorGate(gate.clone()))
        .await;
    let mutation = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(
                    route_key,
                    "http://replacement-add.example".to_owned(),
                    Map::new(),
                )
                .await
        })
    };
    gate.wait_until_arrived().await;
    let observed = Utc::now() + chrono::Duration::seconds(60);
    assert!(registry.observe_activity(&route_key, observed));
    gate.release();

    mutation.await.unwrap().unwrap();

    assert_eq!(
        fixture.routes().await["/dynamic-add-floor"].last_activity,
        observed
    );
    let requests = fixture.requests().await;
    let bodies: Vec<Value> = requests
        .iter()
        .filter(|request| request.method == http::Method::PUT)
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    let retry = &bodies[bodies.len() - 2..];
    assert_eq!(retry[0]["route"], retry[1]["route"]);
    assert_eq!(retry[1]["activityFloor"], chp_timestamp(observed));

    fixture.restart().await;
    let reconnected = sidecar_store(&fixture).await;
    assert_eq!(
        reconnected.snapshot().await.unwrap()[&route_key].last_activity,
        observed
    );
}

#[tokio::test]
async fn sidecar_preserving_put_never_regresses_existing_persisted_activity() {
    let mut fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    let route_key = key("/persisted-floor");
    let mut newer = route("http://independent.example");
    newer.last_activity = Utc.timestamp_opt(100, 0).unwrap();
    store.put(route_key.clone(), newer.clone()).await.unwrap();
    let mut candidate = route("http://replacement.example");
    candidate.last_activity = Utc.timestamp_opt(5, 0).unwrap();

    let committed = store
        .put_preserving_activity(
            route_key.clone(),
            candidate.clone(),
            ActivityFloor::fixed(Some(Utc.timestamp_opt(10, 0).unwrap())),
        )
        .await
        .unwrap();

    let mut expected = candidate;
    expected.last_activity = newer.last_activity;
    assert_eq!(committed, expected);
    assert_eq!(fixture.routes().await["/persisted-floor"], expected);

    fixture.restart().await;
    let reconnected = sidecar_store(&fixture).await;
    assert_eq!(reconnected.snapshot().await.unwrap()[&route_key], expected);
}

#[tokio::test]
async fn sidecar_registry_without_local_route_preserves_independent_writer_activity() {
    let fixture = SidecarFixture::start().await;
    let registry_store = Arc::new(sidecar_store(&fixture).await);
    let registry = RouteRegistry::load(registry_store).await.unwrap();
    let independent = sidecar_store(&fixture).await;
    let route_key = key("/independent-only");
    let mut independent_record = route("http://independent.example");
    independent_record.last_activity = Utc.timestamp_opt(100, 0).unwrap();
    independent
        .put(route_key.clone(), independent_record)
        .await
        .unwrap();
    let mut candidate = route("http://replacement.example");
    candidate.last_activity = Utc.timestamp_opt(5, 0).unwrap();
    candidate
        .extra
        .insert("revision".to_owned(), json!("replacement"));

    registry
        .put(route_key.clone(), candidate.clone())
        .await
        .unwrap();

    let mut expected = candidate;
    expected.last_activity = Utc.timestamp_opt(100, 0).unwrap();
    assert_eq!(registry.get(&route_key).as_ref(), Some(&expected));
    assert_eq!(fixture.routes().await["/independent-only"], expected);
    let requests = fixture.requests().await;
    let operation = requests
        .iter()
        .rev()
        .find(|request| request.method == http::Method::PUT)
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .unwrap();
    assert_eq!(operation["operation"], "put_preserving_activity");
}

#[tokio::test]
async fn sidecar_registry_caches_exact_server_selected_replacement() {
    let mut fixture = SidecarFixture::start().await;
    let independent = sidecar_store(&fixture).await;
    let route_key = key("/exact-committed");
    let mut cached = route("http://a.example");
    cached.last_activity = Utc.timestamp_opt(10, 0).unwrap();
    independent
        .put(route_key.clone(), cached.clone())
        .await
        .unwrap();
    let registry_store = Arc::new(sidecar_store(&fixture).await);
    let registry = RouteRegistry::load(registry_store).await.unwrap();
    cached.last_activity = Utc.timestamp_opt(100, 0).unwrap();
    independent.put(route_key.clone(), cached).await.unwrap();
    let mut replacement = route("http://b.example");
    replacement.last_activity = Utc.timestamp_opt(5, 0).unwrap();
    replacement.extra.insert("generation".to_owned(), json!(2));

    registry
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();

    let mut expected = replacement;
    expected.last_activity = Utc.timestamp_opt(100, 0).unwrap();
    assert_eq!(registry.get(&route_key).as_ref(), Some(&expected));
    assert_eq!(fixture.routes().await["/exact-committed"], expected);

    fixture.restart().await;
    let reloaded = RouteRegistry::load(Arc::new(sidecar_store(&fixture).await))
        .await
        .unwrap();
    assert_eq!(reloaded.get(&route_key).as_ref(), Some(&expected));
}

#[tokio::test]
async fn sidecar_put_unusable_postcommit_acknowledgments_seal_registry() {
    for fault in [
        SidecarFault::MissingProtocol,
        SidecarFault::DuplicateProtocol,
        SidecarFault::WrongProtocol,
        SidecarFault::WrongContentType,
        SidecarFault::WrongSuccessStatus,
        SidecarFault::MalformedBody,
        SidecarFault::OversizedHeaders,
        SidecarFault::TooManyHeaders,
        SidecarFault::NonEmptyNoContent,
        SidecarFault::MissingLastActivity,
        SidecarFault::DuplicateLastActivity,
        SidecarFault::MalformedLastActivity,
        SidecarFault::NoncanonicalLastActivity,
    ] {
        let fixture = SidecarFixture::start().await;
        let store = Arc::new(sidecar_store(&fixture).await);
        let registry = RouteRegistry::load(store).await.unwrap();
        registry
            .put(key("/cached"), route("http://cached-secret.example"))
            .await
            .unwrap();
        fixture.push_fault(SIDECAR_PUT, fault.clone()).await;

        let result = registry
            .put(key("/uncertain"), route("http://body-secret.example"))
            .await;
        assert!(
            result.is_err(),
            "fault unexpectedly acknowledged: {fault:?}"
        );
        let error = result.unwrap_err();

        assert_indeterminate_and_sealed(&error, "put", &registry);
        assert!(registry.get(&key("/cached")).is_none());
        let surfaces = format!("{error} {error:?}");
        assert!(!surfaces.contains("cached-secret"));
        assert!(!surfaces.contains("body-secret"));
    }
}

#[tokio::test]
async fn sidecar_patch_unusable_postcommit_acknowledgments_seal_registry() {
    for fault in [
        SidecarFault::MissingProtocol,
        SidecarFault::DuplicateProtocol,
        SidecarFault::WrongProtocol,
        SidecarFault::WrongContentType,
        SidecarFault::WrongSuccessStatus,
        SidecarFault::MalformedBody,
        SidecarFault::OversizedHeaders,
        SidecarFault::TooManyHeaders,
        SidecarFault::NonEmptyNoContent,
    ] {
        let fixture = SidecarFixture::start().await;
        let store = Arc::new(sidecar_store(&fixture).await);
        let registry = RouteRegistry::load(store).await.unwrap();
        registry
            .put(key("/activity"), route("http://activity-secret.example"))
            .await
            .unwrap();
        fixture.push_fault(SIDECAR_ACTIVITY, fault.clone()).await;

        let result = registry
            .update_activity(&key("/activity"), Utc.timestamp_opt(80, 0).unwrap())
            .await;
        assert!(
            result.is_err(),
            "fault unexpectedly acknowledged: {fault:?}"
        );
        let error = result.unwrap_err();

        assert_indeterminate_and_sealed(&error, "update_activity", &registry);
        assert!(registry.get(&key("/activity")).is_none());
    }
}

#[tokio::test]
async fn sidecar_delete_unusable_postcommit_replies_seal_and_hide_cached_state() {
    for fault in [
        SidecarFault::MissingProtocol,
        SidecarFault::WrongProtocol,
        SidecarFault::WrongContentType,
        SidecarFault::WrongSuccessStatus,
        SidecarFault::MalformedBody,
        SidecarFault::TruncatedBody,
        SidecarFault::OversizedBody,
        SidecarFault::OversizedHeaders,
        SidecarFault::TooManyHeaders,
        SidecarFault::NonEmptyNotFound,
    ] {
        let fixture = SidecarFixture::start().await;
        let store = Arc::new(sidecar_store(&fixture).await);
        let registry = RouteRegistry::load(store).await.unwrap();
        registry
            .put(key("/delete"), route("http://delete-secret.example"))
            .await
            .unwrap();
        fixture.push_fault(SIDECAR_DELETE, fault).await;

        let error = registry.delete(&key("/delete")).await.unwrap_err();

        assert_indeterminate_and_sealed(&error, "delete", &registry);
        assert!(registry.resolve("/delete/path").is_none());
        assert!(!registry.observe_activity(&key("/delete"), Utc.timestamp_opt(90, 0).unwrap()));
        assert!(!format!("{error} {error:?}").contains("delete-secret"));
    }
}

#[tokio::test]
async fn sidecar_delete_strict_decoder_rejects_duplicate_fields_and_noncanonical_time() {
    for body in [
        br#"{"target":"http://first.example","target":"http://secret.example","last_activity":"1970-01-01T00:00:01.000000000Z"}"#.to_vec(),
        br#"{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000Z","last_activity":"1970-01-01T00:00:02.000000000Z"}"#.to_vec(),
        br#"{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01.000000000Z","owner":"first","owner":"secret-owner"}"#.to_vec(),
        br#"{"target":"http://safe.example","last_activity":"1970-01-01T00:00:01+00:00"}"#.to_vec(),
        br#"{"target":42,"last_activity":"1970-01-01T00:00:01.000000000Z"}"#.to_vec(),
    ] {
        let fixture = SidecarFixture::start().await;
        let store = Arc::new(sidecar_store(&fixture).await);
        let registry = RouteRegistry::load(store).await.unwrap();
        registry
            .put(key("/strict-delete"), route("http://prior.example"))
            .await
            .unwrap();
        fixture
            .push_fault(SIDECAR_DELETE, SidecarFault::RawBody(body))
            .await;

        let error = registry.delete(&key("/strict-delete")).await.unwrap_err();

        assert_indeterminate_and_sealed(&error, "delete", &registry);
        let surfaces = format!("{error} {error:?}");
        assert!(!surfaces.contains("secret.example"));
        assert!(!surfaces.contains("secret-owner"));
    }
}

#[tokio::test]
async fn sidecar_mutation_precommit_timeout_never_late_commits() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    fixture.push_fault(SIDECAR_PUT, SidecarFault::Timeout).await;

    let error = store
        .put(key("/precommit-timeout"), route("http://timeout.example"))
        .await
        .unwrap_err();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(matches!(
        error,
        StoreError::Indeterminate { operation: "put" }
    ));
    assert!(fixture.routes().await.is_empty());
    assert_eq!(fixture.request_count(SIDECAR_PUT).await, 1);
}

fn raw_no_content_acknowledgment(headers: &[&str], late_body: &[u8]) -> Vec<u8> {
    let mut response = b"HTTP/1.1 204 No Content\r\n".to_vec();
    for header in headers {
        response.extend_from_slice(header.as_bytes());
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response.extend_from_slice(late_body);
    response
}

#[tokio::test]
async fn sidecar_raw_204_mutation_acknowledgment_faults_are_indeterminate_and_seal() {
    let cases = [
        raw_no_content_acknowledgment(
            &[
                "X-Store-Protocol: v1",
                "X-Store-Protocol: v1",
                "X-Store-Last-Activity: 1970-01-01T00:00:01.000000000Z",
            ],
            b"",
        ),
        raw_no_content_acknowledgment(
            &[
                "X-Store-Protocol: v1",
                "X-Store-Last-Activity: 1970-01-01T00:00:01.000000000Z",
                "Content-Length: 0",
                "Content-Length: 0",
            ],
            b"",
        ),
        raw_no_content_acknowledgment(
            &[
                "X-Store-Protocol: v1",
                "X-Store-Last-Activity: 1970-01-01T00:00:01.000000000Z",
                "Transfer-Encoding: chunked",
            ],
            b"0\r\n\r\n",
        ),
        raw_no_content_acknowledgment(
            &[
                "X-Store-Protocol: v1",
                "X-Store-Last-Activity: 1970-01-01T00:00:01.000000000Z",
                "Content-Length: 29",
            ],
            b"partial",
        ),
        raw_no_content_acknowledgment(
            &[
                "X-Store-Protocol: v1",
                "X-Store-Last-Activity: 1970-01-01T00:00:01.000000000Z",
                "Content-Length: 4194305",
            ],
            b"",
        ),
    ];

    for acknowledgment in cases {
        for operation in [SIDECAR_PUT, SIDECAR_ACTIVITY] {
            let fixture = RawMutationAckFixture::start(acknowledgment.clone()).await;
            let config = SidecarConfig::new(fixture.base_url())
                .with_connect_timeout(Duration::from_millis(100))
                .with_request_timeout(Duration::from_millis(100))
                .with_retry_policy(1, Duration::ZERO, Duration::ZERO);
            let store = Arc::new(SidecarStore::connect(config).await.unwrap());
            let registry = RouteRegistry::load(store).await.unwrap();

            let result = if operation == SIDECAR_PUT {
                registry
                    .put(key("/raw-ack"), route("http://candidate.example"))
                    .await
            } else {
                registry
                    .update_activity(&key("/raw-ack"), Utc.timestamp_opt(10, 0).unwrap())
                    .await
            };

            let error = result.unwrap_err();
            let expected_operation = if operation == SIDECAR_PUT {
                "put"
            } else {
                "update_activity"
            };
            assert_indeterminate_and_sealed(&error, expected_operation, &registry);
        }
    }
}

#[tokio::test]
async fn sidecar_replacement_lost_reply_with_independent_writer_requires_reload() {
    let mut fixture = SidecarFixture::start().await;
    let registry_store = Arc::new(
        SidecarStore::connect(
            sidecar_config(&fixture).with_request_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap(),
    );
    let registry = RouteRegistry::load(registry_store).await.unwrap();
    let route_key = key("/writer-race");
    registry
        .put(route_key.clone(), route("http://initial.example"))
        .await
        .unwrap();
    let gate = FaultGate::new();
    fixture
        .push_fault(SIDECAR_PUT, SidecarFault::LoseReplyGate(gate.clone()))
        .await;
    let mutation = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .put(route_key, route("http://lost-reply.example"))
                .await
        })
    };
    gate.wait_until_arrived().await;
    assert!(!mutation.is_finished());
    let independent = sidecar_store(&fixture).await;
    independent
        .put(
            route_key.clone(),
            route("http://authoritative-writer.example"),
        )
        .await
        .unwrap();
    assert!(!mutation.is_finished());
    gate.release();

    let error = mutation.await.unwrap().unwrap_err();

    assert_indeterminate_and_sealed(&error, "put", &registry);
    fixture.restart().await;
    let reloaded = RouteRegistry::load(Arc::new(sidecar_store(&fixture).await))
        .await
        .unwrap();
    assert_eq!(
        reloaded.get(&route_key).unwrap().target,
        "http://authoritative-writer.example"
    );
    assert_eq!(reloaded.consistency_status(), ConsistencyStatus::Consistent);
}

#[tokio::test]
async fn sidecar_delete_lost_reply_with_independent_writer_requires_reload() {
    let mut fixture = SidecarFixture::start().await;
    let registry_store = Arc::new(
        SidecarStore::connect(
            sidecar_config(&fixture).with_request_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap(),
    );
    let registry = RouteRegistry::load(registry_store).await.unwrap();
    let route_key = key("/delete-race");
    registry
        .put(route_key.clone(), route("http://initial-delete.example"))
        .await
        .unwrap();
    let gate = FaultGate::new();
    fixture
        .push_fault(SIDECAR_DELETE, SidecarFault::LoseReplyGate(gate.clone()))
        .await;
    let mutation = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move { registry.delete(&route_key).await })
    };
    gate.wait_until_arrived().await;
    assert!(!mutation.is_finished());
    let independent = sidecar_store(&fixture).await;
    independent
        .put(
            route_key.clone(),
            route("http://authoritative-after-delete.example"),
        )
        .await
        .unwrap();
    assert!(!mutation.is_finished());
    gate.release();

    let error = mutation.await.unwrap().unwrap_err();

    assert_indeterminate_and_sealed(&error, "delete", &registry);
    fixture.restart().await;
    let reloaded = RouteRegistry::load(Arc::new(sidecar_store(&fixture).await))
        .await
        .unwrap();
    assert_eq!(
        reloaded.get(&route_key).unwrap().target,
        "http://authoritative-after-delete.example"
    );
}

#[tokio::test]
async fn sidecar_delete_5xx_retries_without_double_delete_and_returns_exact_prior() {
    let fixture = SidecarFixture::start().await;
    let store = sidecar_store(&fixture).await;
    let expected = route("http://delete-retry.example");
    store
        .put(key("/delete-retry"), expected.clone())
        .await
        .unwrap();
    fixture
        .push_fault(SIDECAR_DELETE, SidecarFault::ServerError)
        .await;
    fixture
        .push_fault(SIDECAR_DELETE, SidecarFault::ServerError)
        .await;

    let deleted = store.delete(&key("/delete-retry")).await.unwrap();

    assert_eq!(deleted, Some(expected));
    assert_eq!(fixture.request_count(SIDECAR_DELETE).await, 3);
    assert!(fixture.routes().await.is_empty());
}

#[test]
fn sidecar_store_error_display_contract_is_exact_and_redacted() {
    let backend = StoreError::Backend {
        operation: "snapshot",
    };
    let corrupt = StoreError::CorruptData {
        operation: "snapshot",
        key: "<response>".to_owned(),
    };

    assert_eq!(backend.to_string(), "Route store snapshot operation failed");
    assert_eq!(
        corrupt.to_string(),
        "Route store snapshot found a corrupt route record for \"<response>\""
    );
}

fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL must name the disposable Redis")
}

fn redis_hash_key(test_name: &str) -> String {
    format!("{DEFAULT_REDIS_ROUTE_KEY}:test:{test_name}")
}

async fn redis_store(test_name: &str) -> RedisStore {
    RedisStore::connect(RedisStoreConfig::new(redis_url()).with_key(redis_hash_key(test_name)))
        .await
        .unwrap()
}

async fn clear_redis_hash(test_name: &str) {
    let client = redis::Client::open(redis_url()).unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let _: usize = connection.del(redis_hash_key(test_name)).await.unwrap();
}

async fn redis_client_ids(connection: &mut redis::aio::MultiplexedConnection) -> BTreeSet<u64> {
    let clients: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query_async(connection)
        .await
        .unwrap();
    clients
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("id="))
                .map(|id| id.parse().unwrap())
        })
        .collect()
}

struct SuppressMutationReplyProxy {
    url: String,
    committed: Option<tokio::sync::oneshot::Receiver<()>>,
    task: tokio::task::JoinHandle<()>,
}

struct PauseReplyProxy {
    url: String,
    paused: Option<tokio::sync::oneshot::Receiver<()>>,
    release: Arc<Semaphore>,
    exec_responses: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl PauseReplyProxy {
    async fn start(command: &str) -> Self {
        let backend = url::Url::parse(&redis_url()).unwrap();
        let backend_address = format!(
            "{}:{}",
            backend.host_str().unwrap(),
            backend.port_or_known_default().unwrap()
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let command = Arc::new(command.as_bytes().to_ascii_uppercase());
        let claimed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Semaphore::new(0));
        let exec_responses = Arc::new(AtomicUsize::new(0));
        let (paused_tx, paused) = tokio::sync::oneshot::channel();
        let paused_tx = Arc::new(std::sync::Mutex::new(Some(paused_tx)));
        let task = {
            let release = Arc::clone(&release);
            let exec_responses = Arc::clone(&exec_responses);
            tokio::spawn(async move {
                loop {
                    let (client, _) = listener.accept().await.unwrap();
                    let server = TcpStream::connect(&backend_address).await.unwrap();
                    let command = Arc::clone(&command);
                    let claimed = Arc::clone(&claimed);
                    let release = Arc::clone(&release);
                    let paused_tx = Arc::clone(&paused_tx);
                    let exec_responses = Arc::clone(&exec_responses);
                    tokio::spawn(async move {
                        pause_reply_connection(
                            client,
                            server,
                            command,
                            claimed,
                            paused_tx,
                            release,
                            exec_responses,
                        )
                        .await;
                    });
                }
            })
        };
        Self {
            url: format!("redis://{address}/"),
            paused: Some(paused),
            release,
            exec_responses,
            task,
        }
    }

    async fn wait_until_paused(&mut self) {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.paused.take().expect("pause signal is consumed once"),
        )
        .await
        .expect("matching Redis reply was not paused")
        .unwrap();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }

    fn exec_responses(&self) -> usize {
        self.exec_responses.load(Ordering::SeqCst)
    }
}

impl Drop for PauseReplyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SuppressMutationReplyProxy {
    async fn start(commands: &[&str]) -> Self {
        let backend = url::Url::parse(&redis_url()).unwrap();
        let backend_address = format!(
            "{}:{}",
            backend.host_str().unwrap(),
            backend.port_or_known_default().unwrap()
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let commands: Arc<Vec<Vec<u8>>> = Arc::new(
            commands
                .iter()
                .map(|command| command.as_bytes().to_ascii_uppercase())
                .collect(),
        );
        let claimed = Arc::new(AtomicBool::new(false));
        let (committed_tx, committed) = tokio::sync::oneshot::channel();
        let committed_tx = Arc::new(std::sync::Mutex::new(Some(committed_tx)));
        let task = tokio::spawn(async move {
            loop {
                let (client, _) = listener.accept().await.unwrap();
                let server = TcpStream::connect(&backend_address).await.unwrap();
                let commands = Arc::clone(&commands);
                let claimed = Arc::clone(&claimed);
                let committed_tx = Arc::clone(&committed_tx);
                tokio::spawn(async move {
                    proxy_connection(client, server, commands, claimed, committed_tx).await;
                });
            }
        });
        Self {
            url: format!("redis://{address}/"),
            committed: Some(committed),
            task,
        }
    }

    async fn wait_for_committed_mutation(&mut self) {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.committed
                .take()
                .expect("commit signal is consumed once"),
        )
        .await
        .expect("matching Redis mutation reply was not suppressed")
        .unwrap();
    }
}

async fn proxy_connection(
    mut client: TcpStream,
    mut server: TcpStream,
    commands: Arc<Vec<Vec<u8>>>,
    claimed: Arc<AtomicBool>,
    committed: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) {
    let mut client_read = [0_u8; 16 * 1024];
    let mut server_read = [0_u8; 16 * 1024];
    let mut requests = Vec::new();
    let mut responses = Vec::new();
    let mut suppressions = VecDeque::new();
    loop {
        tokio::select! {
            read = client.read(&mut client_read) => {
                let Ok(read) = read else { break };
                if read == 0 { break; }
                requests.extend_from_slice(&client_read[..read]);
                while let Some(length) = resp_frame_len(&requests) {
                    let frame: Vec<_> = requests.drain(..length).collect();
                    let matches = resp_command(&frame)
                        .is_some_and(|command| commands.iter().any(|candidate| candidate == command));
                    let suppress = matches
                        && claimed
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok();
                    suppressions.push_back(suppress);
                    if server.write_all(&frame).await.is_err() { return; }
                }
            }
            read = server.read(&mut server_read) => {
                let Ok(read) = read else { break };
                if read == 0 { break; }
                responses.extend_from_slice(&server_read[..read]);
                while let Some(length) = resp_frame_len(&responses) {
                    let frame: Vec<_> = responses.drain(..length).collect();
                    let suppress = suppressions.pop_front().unwrap_or(false);
                    if suppress {
                        if let Some(sender) = committed.lock().unwrap().take() {
                            let _ = sender.send(());
                        }
                    } else if client.write_all(&frame).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

async fn pause_reply_connection(
    mut client: TcpStream,
    mut server: TcpStream,
    pause_command: Arc<Vec<u8>>,
    claimed: Arc<AtomicBool>,
    paused: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    release: Arc<Semaphore>,
    exec_responses: Arc<AtomicUsize>,
) {
    let mut client_read = [0_u8; 16 * 1024];
    let mut server_read = [0_u8; 16 * 1024];
    let mut requests = Vec::new();
    let mut responses = Vec::new();
    let mut commands = VecDeque::new();
    loop {
        tokio::select! {
            read = client.read(&mut client_read) => {
                let Ok(read) = read else { break };
                if read == 0 { break; }
                requests.extend_from_slice(&client_read[..read]);
                while let Some(length) = resp_frame_len(&requests) {
                    let frame: Vec<_> = requests.drain(..length).collect();
                    commands.push_back(resp_command(&frame).map(<[u8]>::to_vec));
                    if server.write_all(&frame).await.is_err() { return; }
                }
            }
            read = server.read(&mut server_read) => {
                let Ok(read) = read else { break; };
                if read == 0 { break; }
                responses.extend_from_slice(&server_read[..read]);
                while let Some(length) = resp_frame_len(&responses) {
                    let frame: Vec<_> = responses.drain(..length).collect();
                    let command = commands.pop_front().flatten();
                    if command.as_deref() == Some(b"EXEC") {
                        exec_responses.fetch_add(1, Ordering::SeqCst);
                    }
                    let pause = command.as_deref() == Some(pause_command.as_slice())
                        && claimed
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok();
                    if pause {
                        if let Some(sender) = paused.lock().unwrap().take() {
                            let _ = sender.send(());
                        }
                        if release.acquire().await.is_err() { return; }
                    }
                    if client.write_all(&frame).await.is_err() { return; }
                }
            }
        }
    }
}

fn resp_frame_len(input: &[u8]) -> Option<usize> {
    resp_frame_end(input, 0)
}

fn resp_frame_end(input: &[u8], start: usize) -> Option<usize> {
    let kind = *input.get(start)?;
    let line_end = input[start + 1..]
        .windows(2)
        .position(|bytes| bytes == b"\r\n")?
        + start
        + 1;
    let after_line = line_end + 2;
    match kind {
        b'+' | b'-' | b':' | b',' | b'(' | b'#' => Some(after_line),
        b'_' => Some(after_line),
        b'$' | b'!' | b'=' => {
            let length = std::str::from_utf8(&input[start + 1..line_end])
                .ok()?
                .parse::<isize>()
                .ok()?;
            if length < 0 {
                Some(after_line)
            } else {
                let end = after_line.checked_add(length as usize)?.checked_add(2)?;
                (input.get(end - 2..end) == Some(b"\r\n")).then_some(end)
            }
        }
        b'*' | b'~' | b'>' | b'%' | b'|' => {
            let mut count = std::str::from_utf8(&input[start + 1..line_end])
                .ok()?
                .parse::<isize>()
                .ok()?;
            if count < 0 {
                return Some(after_line);
            }
            if matches!(kind, b'%' | b'|') {
                count = count.checked_mul(2)?;
            }
            let mut end = after_line;
            for _ in 0..count {
                end = resp_frame_end(input, end)?;
            }
            Some(end)
        }
        _ => None,
    }
}

fn resp_command(frame: &[u8]) -> Option<&[u8]> {
    if frame.first() != Some(&b'*') {
        return None;
    }
    let array_header = frame.windows(2).position(|bytes| bytes == b"\r\n")? + 2;
    if frame.get(array_header) != Some(&b'$') {
        return None;
    }
    let length_end = frame[array_header + 1..]
        .windows(2)
        .position(|bytes| bytes == b"\r\n")?
        + array_header
        + 1;
    let length = std::str::from_utf8(&frame[array_header + 1..length_end])
        .ok()?
        .parse::<usize>()
        .ok()?;
    let command_start = length_end + 2;
    frame.get(command_start..command_start + length)
}

impl Drop for SuppressMutationReplyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fault_proxy_store(
    test_name: &str,
    commands: &[&str],
) -> (RedisStore, SuppressMutationReplyProxy) {
    let proxy = SuppressMutationReplyProxy::start(commands).await;
    let store = RedisStore::connect(
        RedisStoreConfig::new(proxy.url.clone())
            .with_key(redis_hash_key(test_name))
            .with_operation_timeout(Duration::from_millis(500)),
    )
    .await
    .unwrap();
    (store, proxy)
}

#[tokio::test]
#[serial(redis)]
async fn redis_store_satisfies_backend_neutral_contract() {
    const TEST_NAME: &str = "shared-contract";
    clear_redis_hash(TEST_NAME).await;
    assert_store_contract(Arc::new(redis_store(TEST_NAME).await)).await;
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_overwrite_delete_missing_and_metadata_round_trip() {
    const TEST_NAME: &str = "metadata";
    clear_redis_hash(TEST_NAME).await;
    let store = redis_store(TEST_NAME).await;
    let route_key = key("//user/alice///");
    let original = RouteData {
        target: "http://original.example/base".to_owned(),
        last_activity: Utc.timestamp_opt(123, 456_000_000).unwrap(),
        extra: Map::from_iter([
            ("owner".to_owned(), json!("jupyterhub")),
            (
                "nested".to_owned(),
                json!({"roles": ["user", "admin"], "enabled": true}),
            ),
            ("nullable".to_owned(), json!(null)),
        ]),
    };
    store.put(route_key.clone(), original).await.unwrap();

    let replacement = RouteData {
        target: "https://replacement.example/path?query=1".to_owned(),
        last_activity: Utc.timestamp_opt(987, 654_000_000).unwrap(),
        extra: Map::from_iter([
            ("owner".to_owned(), json!("replacement")),
            (
                "arbitrary".to_owned(),
                json!({"deep": {"array": [1, "two", false]}}),
            ),
        ]),
    };
    store
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();

    assert_eq!(store.snapshot().await.unwrap().len(), 1);
    assert_eq!(store.snapshot().await.unwrap()[&route_key], replacement);
    let client = redis::Client::open(redis_url()).unwrap();
    let mut raw_connection = client.get_multiplexed_async_connection().await.unwrap();
    let raw_records: Vec<(String, String)> = raw_connection
        .hgetall(redis_hash_key(TEST_NAME))
        .await
        .unwrap();
    assert_eq!(raw_records.len(), 1);
    assert_eq!(raw_records[0].0, route_key.as_str());
    let raw: serde_json::Value = serde_json::from_str(&raw_records[0].1).unwrap();
    assert_eq!(raw["target"], replacement.target);
    assert_eq!(raw["last_activity"], "1970-01-01T00:16:27.654000000Z");
    assert_eq!(raw["owner"], "replacement");
    assert_eq!(
        raw["arbitrary"],
        json!({"deep": {"array": [1, "two", false]}})
    );
    assert_eq!(raw.as_object().unwrap().len(), 4);
    assert_eq!(store.delete(&key("/missing")).await.unwrap(), None);
    assert_eq!(store.delete(&route_key).await.unwrap(), Some(replacement));
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_corrupt_snapshot_is_typed_redacted_and_never_published() {
    const TEST_NAME: &str = "corrupt-snapshot";
    clear_redis_hash(TEST_NAME).await;
    let hash_key = redis_hash_key(TEST_NAME);
    let client = redis::Client::open(redis_url()).unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let corrupt = r#"{"target":"http://secret.example","last_activity":false}"#;
    let _: usize = connection
        .hset(&hash_key, "/corrupt", corrupt)
        .await
        .unwrap();

    let store = Arc::new(redis_store(TEST_NAME).await);
    let error = match RouteRegistry::load(store).await {
        Ok(_) => panic!("corrupt Redis data must prevent registry publication"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        StoreError::CorruptData {
            operation: "snapshot",
            ..
        }
    ));
    let rendered = error.to_string();
    assert!(!rendered.contains(corrupt));
    assert!(!rendered.contains("secret.example"));
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_corrupt_watched_record_poisoned_connection_is_not_reused() {
    const TEST_NAME: &str = "corrupt-watched-record";
    clear_redis_hash(TEST_NAME).await;
    let client = redis::Client::open(redis_url()).unwrap();
    let mut administrator = client.get_multiplexed_async_connection().await.unwrap();
    let before = redis_client_ids(&mut administrator).await;
    let store = redis_store(TEST_NAME).await;
    let connected = redis_client_ids(&mut administrator).await;
    let store_id = *connected.difference(&before).next().unwrap();
    let route_key = key("/corrupt-watched");
    let _: usize = administrator
        .hset(
            redis_hash_key(TEST_NAME),
            route_key.as_str(),
            r#"{"target":false}"#,
        )
        .await
        .unwrap();

    let error = store
        .update_activity(&route_key, Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::CorruptData {
            operation: "update_activity",
            ..
        }
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while redis_client_ids(&mut administrator)
        .await
        .contains(&store_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a connection that may still be WATCHing must be replaced"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let replacement = route("http://replacement-after-corruption.example");
    store
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();
    assert_eq!(store.snapshot().await.unwrap()[&route_key], replacement);
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_unavailable_startup_is_bounded_typed_and_redacts_credentials() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let secret = "redis-test-password-never-render";
    let url = format!("redis://default:{secret}@127.0.0.1:{port}/");
    let config = RedisStoreConfig::new(url);
    assert!(!format!("{config:?}").contains(secret));

    let error = tokio::time::timeout(Duration::from_secs(3), RedisStore::connect(config))
        .await
        .expect("startup must have a finite connection bound")
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "connect",
            ..
        }
    ));
    assert!(!error.to_string().contains(secret));
}

#[tokio::test]
#[serial(redis)]
async fn redis_disconnect_during_mutation_returns_typed_error_without_applying_write() {
    const TEST_NAME: &str = "disconnect";
    clear_redis_hash(TEST_NAME).await;
    let client = redis::Client::open(redis_url()).unwrap();
    let mut administrator = client.get_multiplexed_async_connection().await.unwrap();
    let before = redis_client_ids(&mut administrator).await;
    let store = redis_store(TEST_NAME).await;
    let after = redis_client_ids(&mut administrator).await;
    let store_ids: Vec<_> = after.difference(&before).copied().collect();
    assert_eq!(store_ids.len(), 1, "exactly one store client must connect");
    let killed: usize = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(store_ids[0])
        .query_async(&mut administrator)
        .await
        .unwrap();
    assert_eq!(killed, 1, "only the exact store connection is disconnected");

    let error = store
        .put(key("/not-applied"), route("http://not-applied.example"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::Backend {
            operation: "put",
            ..
        }
    ));

    let mut verification = client.get_multiplexed_async_connection().await.unwrap();
    let exists: bool = verification
        .hexists(redis_hash_key(TEST_NAME), "/not-applied")
        .await
        .unwrap();
    assert!(!exists);
    clear_redis_hash(TEST_NAME).await;
}

async fn independent_redis_put(test_name: &str, route_key: &RouteKey, data: &RouteData) {
    let client = redis::Client::open(redis_url()).unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let encoded = serde_json::to_string(data).unwrap();
    let _: usize = connection
        .hset(redis_hash_key(test_name), route_key.as_str(), encoded)
        .await
        .unwrap();
}

async fn independent_redis_delete(test_name: &str, route_key: &RouteKey) {
    let client = redis::Client::open(redis_url()).unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let _: usize = connection
        .hdel(redis_hash_key(test_name), route_key.as_str())
        .await
        .unwrap();
}

fn assert_indeterminate(error: StoreError, operation: &'static str) {
    assert!(matches!(
        error,
        StoreError::Indeterminate {
            operation: actual
        } if actual == operation
    ));
}

#[tokio::test]
#[serial(redis)]
async fn redis_lost_replacement_reply_with_intervening_writer_seals_registry() {
    const TEST_NAME: &str = "indeterminate-replacement-writer";
    clear_redis_hash(TEST_NAME).await;
    let route_key = key("/uncertain");
    let original = route("http://original.example");
    independent_redis_put(TEST_NAME, &route_key, &original).await;
    let (store, mut proxy) = fault_proxy_store(TEST_NAME, &["EXEC"]).await;
    let registry = RouteRegistry::load(Arc::new(store)).await.unwrap();

    let replacement = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(
                    route_key,
                    "http://attempted-replacement.example".to_owned(),
                    Map::new(),
                )
                .await
        })
    };
    proxy.wait_for_committed_mutation().await;
    assert!(!replacement.is_finished());
    assert_eq!(registry.get(&route_key), Some(original));
    let queued = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.delete(&key("/queued-missing")).await })
    };
    let writer = route("http://independent-writer-b.example");
    independent_redis_put(TEST_NAME, &route_key, &writer).await;

    assert_indeterminate(replacement.await.unwrap().unwrap_err(), "add");
    assert_eq!(
        queued.await.unwrap().unwrap_err().to_string(),
        MUTATION_INDETERMINATE_SEALED_ERROR
    );
    assert!(registry.mutation_status().sealed);
    assert_eq!(registry.mutation_status().seal, MutationSeal::Indeterminate);
    assert_eq!(
        registry.consistency_status(),
        ConsistencyStatus::Indeterminate
    );
    assert_eq!(registry.get(&route_key), None);
    assert!(registry.all().is_empty());
    assert!(registry.resolve("/uncertain/child").is_none());
    assert!(!registry.observe_activity(&route_key, Utc::now()));
    assert_eq!(
        registry
            .delete(&key("/subsequent-missing"))
            .await
            .unwrap_err()
            .to_string(),
        MUTATION_INDETERMINATE_SEALED_ERROR
    );

    let recovered = RouteRegistry::load(Arc::new(redis_store(TEST_NAME).await))
        .await
        .unwrap();
    assert!(!recovered.mutation_status().sealed);
    assert_eq!(
        recovered.consistency_status(),
        ConsistencyStatus::Consistent
    );
    assert_eq!(recovered.get(&route_key), Some(writer));
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_lost_delete_reply_with_intervening_writer_is_indeterminate() {
    const TEST_NAME: &str = "indeterminate-delete-writer";
    clear_redis_hash(TEST_NAME).await;
    let route_key = key("/uncertain-delete");
    independent_redis_put(TEST_NAME, &route_key, &route("http://delete-prior.example")).await;
    let (store, mut proxy) = fault_proxy_store(TEST_NAME, &["EXEC", "EVAL"]).await;
    let registry = RouteRegistry::load(Arc::new(store)).await.unwrap();

    let deleting = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move { registry.delete(&route_key).await })
    };
    proxy.wait_for_committed_mutation().await;
    assert!(!deleting.is_finished());
    let writer = route("http://independent-writer-b.example");
    independent_redis_put(TEST_NAME, &route_key, &writer).await;

    assert_indeterminate(deleting.await.unwrap().unwrap_err(), "delete");
    assert!(registry.mutation_status().sealed);
    assert_eq!(registry.get(&route_key), None);
    let recovered = RouteRegistry::load(Arc::new(redis_store(TEST_NAME).await))
        .await
        .unwrap();
    assert_eq!(recovered.get(&route_key), Some(writer));
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_lost_delete_reply_with_intervening_absence_is_indeterminate() {
    const TEST_NAME: &str = "indeterminate-delete-absence";
    clear_redis_hash(TEST_NAME).await;
    let route_key = key("/uncertain-delete");
    independent_redis_put(TEST_NAME, &route_key, &route("http://delete-prior.example")).await;
    let (store, mut proxy) = fault_proxy_store(TEST_NAME, &["EXEC", "EVAL"]).await;
    let registry = RouteRegistry::load(Arc::new(store)).await.unwrap();

    let deleting = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move { registry.delete(&route_key).await })
    };
    proxy.wait_for_committed_mutation().await;
    assert!(!deleting.is_finished());
    independent_redis_delete(TEST_NAME, &route_key).await;

    assert_indeterminate(deleting.await.unwrap().unwrap_err(), "delete");
    assert!(registry.mutation_status().sealed);
    let recovered = RouteRegistry::load(Arc::new(redis_store(TEST_NAME).await))
        .await
        .unwrap();
    assert_eq!(recovered.get(&route_key), None);
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_every_dispatched_mutator_reply_loss_is_indeterminate() {
    const TEST_NAME: &str = "all-indeterminate-mutators";
    clear_redis_hash(TEST_NAME).await;
    let route_key = key("/uncertain-mutator");

    let (put, mut proxy) = fault_proxy_store(TEST_NAME, &["HSET"]).await;
    let putting = tokio::spawn(async move {
        put.put(
            key("/uncertain-mutator"),
            route("http://lost-put-reply.example"),
        )
        .await
    });
    proxy.wait_for_committed_mutation().await;
    assert_indeterminate(putting.await.unwrap().unwrap_err(), "put");

    let (replace, mut proxy) = fault_proxy_store(TEST_NAME, &["EXEC"]).await;
    let replacing = {
        let route_key = route_key.clone();
        tokio::spawn(async move {
            replace
                .put_preserving_activity(
                    route_key,
                    route("http://lost-replacement-reply.example"),
                    ActivityFloor::fixed(None),
                )
                .await
        })
    };
    proxy.wait_for_committed_mutation().await;
    assert_indeterminate(replacing.await.unwrap().unwrap_err(), "put");

    let (activity, mut proxy) = fault_proxy_store(TEST_NAME, &["EXEC"]).await;
    let updating = {
        let route_key = route_key.clone();
        tokio::spawn(async move { activity.update_activity(&route_key, Utc::now()).await })
    };
    proxy.wait_for_committed_mutation().await;
    assert_indeterminate(updating.await.unwrap().unwrap_err(), "update_activity");
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_two_stores_retry_forced_exec_contention() {
    const TEST_NAME: &str = "two-store-contention";
    clear_redis_hash(TEST_NAME).await;
    let mut proxy = PauseReplyProxy::start("WATCH").await;
    let first = RedisStore::connect(
        RedisStoreConfig::new(proxy.url.clone())
            .with_key(redis_hash_key(TEST_NAME))
            .with_operation_timeout(Duration::from_secs(2)),
    );
    let first = first.await.unwrap();
    let first_write = tokio::spawn(async move {
        first
            .add(
                key("/contended"),
                "http://first-writer.example".to_owned(),
                Map::new(),
                ActivityFloor::fixed(None),
            )
            .await
    });

    proxy.wait_until_paused().await;
    redis_store(TEST_NAME)
        .await
        .add(
            key("/contended"),
            "http://second-writer.example".to_owned(),
            Map::new(),
            ActivityFloor::fixed(None),
        )
        .await
        .unwrap();
    proxy.release();
    let first_committed = first_write.await.unwrap().unwrap();

    assert_eq!(
        proxy.exec_responses(),
        2,
        "the first EXEC must abort and retry"
    );
    assert_eq!(
        redis_store(TEST_NAME).await.snapshot().await.unwrap()[&key("/contended")],
        first_committed
    );
    clear_redis_hash(TEST_NAME).await;
}

#[tokio::test]
#[serial(redis)]
async fn redis_restart_persistence_recovers_complete_routes_without_clearing() {
    const TEST_NAME: &str = "restart-persistence";
    let Ok(phase) = std::env::var("REDIS_RESTART_PHASE") else {
        for phase in ["writer", "reader"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .arg("redis_restart_persistence_recovers_complete_routes_without_clearing")
                .arg("--exact")
                .arg("--nocapture")
                .env("REDIS_RESTART_PHASE", phase)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "restart {phase} subprocess failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let client = redis::Client::open(redis_url()).unwrap();
        let mut connection = client.get_multiplexed_async_connection().await.unwrap();
        let exists: bool = connection.exists(redis_hash_key(TEST_NAME)).await.unwrap();
        assert!(!exists, "reader phase must clean the stable restart key");
        return;
    };
    let store = Arc::new(redis_store(TEST_NAME).await);
    let route_key = key("//persistent/user///");
    let expected = RouteData {
        target: "http://persistent.example/base".to_owned(),
        last_activity: Utc.timestamp_opt(1_234_567, 890_000_000).unwrap(),
        extra: Map::from_iter([
            ("owner".to_owned(), json!("restart-test")),
            (
                "metadata".to_owned(),
                json!({"survives": ["process", "restart"]}),
            ),
        ]),
    };

    match phase.as_str() {
        "writer" => {
            clear_redis_hash(TEST_NAME).await;
            let registry = RouteRegistry::load(store).await.unwrap();
            assert!(registry.get(&route_key).is_none());
            registry.put(route_key, expected).await.unwrap();
        }
        "reader" => {
            let registry = RouteRegistry::load(store).await.unwrap();
            assert_eq!(registry.get(&route_key), Some(expected));
            clear_redis_hash(TEST_NAME).await;
        }
        phase => panic!("unknown REDIS_RESTART_PHASE {phase:?}"),
    }
}

#[derive(Clone, Debug)]
enum Operation {
    Put(u8, u8),
    Activity(u8, i64),
    Delete(u8),
}

fn operation_strategy() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (0u8..8, any::<u8>()).prop_map(|(key, route)| Operation::Put(key, route)),
        (0u8..8, 0i64..10_000).prop_map(|(key, at)| Operation::Activity(key, at)),
        (0u8..8).prop_map(Operation::Delete),
    ]
}

proptest! {
    #[test]
    fn memory_store_matches_reference_state_machine(
        operations in prop::collection::vec(operation_strategy(), 0..64),
    ) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let store = memory_store();
            let mut reference = BTreeMap::new();

            for operation in operations {
                match operation {
                    Operation::Put(id, route_id) => {
                        let route_key = key(&format!("/route/{id}"));
                        let route_data = route(&format!("http://127.0.0.1:{}/", 10_000 + u16::from(route_id)));
                        let activity_floor = reference
                            .get(&route_key)
                            .map(|route: &RouteData| route.last_activity);
                        let route_data = store
                            .put_preserving_activity(
                                route_key.clone(),
                                route_data,
                                ActivityFloor::fixed(activity_floor),
                            )
                            .await
                            .unwrap();
                        reference.insert(route_key, route_data);
                    }
                    Operation::Activity(id, seconds) => {
                        let route_key = key(&format!("/route/{id}"));
                        let at = Utc.timestamp_opt(seconds, 0).unwrap();
                        store.update_activity(&route_key, at).await.unwrap();
                        if let Some(route_data) = reference.get_mut(&route_key) {
                            route_data.last_activity = at;
                        }
                    }
                    Operation::Delete(id) => {
                        let route_key = key(&format!("/route/{id}"));
                        let actual = store.delete(&route_key).await.unwrap();
                        prop_assert_eq!(actual, reference.remove(&route_key));
                    }
                }

                prop_assert_eq!(store.snapshot().await.unwrap(), reference.clone());
            }

            Ok(())
        })?;
    }
}

struct FailingStore {
    routes: BTreeMap<RouteKey, RouteData>,
}

impl FailingStore {
    fn on_put() -> Self {
        Self::with_routes(BTreeMap::new())
    }

    fn with_routes(routes: BTreeMap<RouteKey, RouteData>) -> Self {
        Self { routes }
    }
}

#[async_trait]
impl Store for FailingStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("injected add failure"))
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        Err(StoreError::message("injected put failure"))
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("injected put failure"))
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        Err(StoreError::message("injected activity failure"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("injected delete failure"))
    }
}

#[tokio::test]
async fn failed_persistence_never_publishes_route() {
    let store = Arc::new(FailingStore::on_put());
    let registry = RouteRegistry::load(store).await.unwrap();
    let result = registry
        .put(key("/user/a"), route("http://127.0.0.1:9000"))
        .await;
    assert!(result.is_err());
    assert!(registry.resolve("/user/a/tree").is_none());
}

#[tokio::test]
async fn failed_atomic_put_changes_no_backend_state_and_fresh_reload_matches() {
    let route_key = key("/user/a");
    let original = route("http://original.example");
    let store = Arc::new(FailingStore::with_routes(BTreeMap::from([(
        route_key.clone(),
        original.clone(),
    )])));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let observed = Utc.timestamp_opt(999, 0).unwrap();
    assert!(registry.observe_activity(&route_key, observed));

    assert!(registry
        .put(route_key.clone(), route("http://replacement.example"))
        .await
        .is_err());
    assert_eq!(store.snapshot().await.unwrap()[&route_key], original);
    let reloaded = RouteRegistry::load(store).await.unwrap();
    assert_eq!(reloaded.get(&route_key), Some(original));
}

struct GatedAtomicAddStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    entered: Semaphore,
    release: Semaphore,
    fail: bool,
}

impl GatedAtomicAddStore {
    fn new(routes: BTreeMap<RouteKey, RouteData>, fail: bool) -> Self {
        Self {
            routes: RwLock::new(routes),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            fail,
        }
    }
}

#[async_trait]
impl Store for GatedAtomicAddStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        if self.fail {
            return Err(StoreError::message("injected atomic add failure"));
        }

        let mut routes = self.routes.write().await;
        let mut data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
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

#[tokio::test]
async fn atomic_add_failure_leaves_backend_and_registry_unchanged() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let store = Arc::new(GatedAtomicAddStore::new(
        BTreeMap::from([(route_key.clone(), original.clone())]),
        true,
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let adding = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(
                    route_key,
                    "http://replacement.example".to_owned(),
                    Map::new(),
                )
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    store.release.add_permits(1);
    assert!(adding.await.unwrap().is_err());

    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&original)
    );
    assert_eq!(registry.get(&route_key), Some(original));
}

#[tokio::test]
async fn cancelling_add_before_commit_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let store = Arc::new(GatedAtomicAddStore::new(
        BTreeMap::from([(route_key.clone(), original.clone())]),
        false,
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let adding = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(
                    route_key,
                    "http://replacement.example".to_owned(),
                    Map::new(),
                )
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    adding.abort();
    assert!(adding.await.unwrap_err().is_cancelled());
    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&original)
    );
    assert_eq!(registry.get(&route_key), Some(original));

    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    let committed = store.snapshot().await.unwrap().remove(&route_key).unwrap();
    assert_eq!(committed.target, "http://replacement.example");
    assert_eq!(registry.get(&route_key), Some(committed));
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GatedMutation {
    Put,
    UpdateActivity,
    Delete,
}

struct GatedMutationStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    gated: GatedMutation,
    entered: Semaphore,
    release: Semaphore,
}

impl GatedMutationStore {
    fn new(gated: GatedMutation, routes: BTreeMap<RouteKey, RouteData>) -> Self {
        Self {
            routes: RwLock::new(routes),
            gated,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    async fn gate(&self, mutation: GatedMutation) {
        if self.gated == mutation {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
    }
}

#[async_trait]
impl Store for GatedMutationStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.gate(GatedMutation::Put).await;
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.gate(GatedMutation::Put).await;
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        self.gate(GatedMutation::UpdateActivity).await;
        if let Some(route) = self.routes.write().await.get_mut(key) {
            route.last_activity = at;
        }
        Ok(())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        self.gate(GatedMutation::Delete).await;
        Ok(self.routes.write().await.remove(key))
    }
}

#[tokio::test]
async fn cancelling_put_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let replacement = route("http://replacement.example");
    let store = Arc::new(GatedMutationStore::new(GatedMutation::Put, BTreeMap::new()));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let caller = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        let replacement = replacement.clone();
        tokio::spawn(async move { registry.put(route_key, replacement).await })
    };

    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    assert_eq!(
        store.snapshot().await.unwrap().get(&route_key),
        Some(&replacement)
    );
    assert_eq!(registry.get(&route_key), Some(replacement));
}

#[tokio::test]
async fn put_samples_activity_after_method_entry_at_atomic_commit_and_survives_reload() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let observed = Utc.timestamp_opt(999, 0).unwrap();
    let store = Arc::new(GatedMutationStore::new(
        GatedMutation::Put,
        BTreeMap::from([(route_key.clone(), original)]),
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let putting = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .put(route_key, route("http://replacement.example"))
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    assert!(registry.observe_activity(&route_key, observed));
    store.release.add_permits(1);
    putting.await.unwrap().unwrap();

    let committed = store.snapshot().await.unwrap()[&route_key].clone();
    assert_eq!(committed.target, "http://replacement.example");
    assert_eq!(committed.last_activity, observed);
    assert_eq!(registry.get(&route_key), Some(committed.clone()));
    let reloaded = RouteRegistry::load(store).await.unwrap();
    assert_eq!(reloaded.get(&route_key), Some(committed));
}

#[tokio::test]
async fn cancelling_activity_update_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let original = route("http://original.example");
    let activity = Utc.timestamp_opt(999, 0).unwrap();
    let store = Arc::new(GatedMutationStore::new(
        GatedMutation::UpdateActivity,
        BTreeMap::from([(route_key.clone(), original)]),
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    assert!(registry.observe_activity(&route_key, activity));
    let caller = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .persist_observed_activity(&route_key, activity)
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    assert_eq!(
        store.snapshot().await.unwrap()[&route_key].last_activity,
        activity
    );
    assert_eq!(registry.get(&route_key).unwrap().last_activity, activity);
}

#[tokio::test]
async fn cancelling_delete_does_not_cancel_the_registry_owned_mutation() {
    let route_key = key("/service");
    let store = Arc::new(GatedMutationStore::new(
        GatedMutation::Delete,
        BTreeMap::from([(route_key.clone(), route("http://original.example"))]),
    ));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let caller = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move { registry.delete(&route_key).await })
    };

    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);

    assert!(!store.snapshot().await.unwrap().contains_key(&route_key));
    assert!(registry.get(&route_key).is_none());
}

struct TimingOutPutStore;

#[async_trait]
impl Store for TimingOutPutStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(BTreeMap::new())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("unused add"))
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
            .await
            .map_err(|_| StoreError::message("backend timeout"))
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
            .await
            .map_err(|_| StoreError::message("backend timeout"))?;
        unreachable!()
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        Err(StoreError::message("unused activity update"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("unused delete"))
    }
}

#[tokio::test]
async fn backend_timeout_finishes_the_mutation_task_and_releases_the_registry() {
    let registry = RouteRegistry::load(Arc::new(TimingOutPutStore))
        .await
        .unwrap();
    let registry_lifetime = Arc::downgrade(&registry);

    let error = registry
        .put(key("/service"), route("http://timeout.example"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "backend timeout");
    drop(registry);

    assert!(
        registry_lifetime.upgrade().is_none(),
        "a completed backend timeout must not leave a detached task retaining the registry"
    );
}

#[tokio::test]
async fn slow_atomic_add_stamps_after_delay_and_publishes_exact_committed_data() {
    let route_key = key("/service");
    let store = Arc::new(GatedAtomicAddStore::new(BTreeMap::new(), false));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let adding = {
        let registry = Arc::clone(&registry);
        let route_key = route_key.clone();
        tokio::spawn(async move {
            registry
                .add(route_key, "http://committed.example".to_owned(), Map::new())
                .await
        })
    };

    store.entered.acquire().await.unwrap().forget();
    let delay_finished_at = Utc::now();
    store.release.add_permits(1);
    adding.await.unwrap().unwrap();

    let committed = store.snapshot().await.unwrap().remove(&route_key).unwrap();
    assert!(committed.last_activity >= delay_finished_at);
    assert_eq!(registry.get(&route_key), Some(committed));
}

fn registry_views(
    registry: &RouteRegistry,
    route_key: &RouteKey,
    request_path: &str,
) -> (
    Option<RouteData>,
    BTreeMap<RouteKey, RouteData>,
    Option<RouteMatch>,
) {
    (
        registry.get(route_key),
        registry.all(),
        registry.resolve(request_path),
    )
}

#[tokio::test]
async fn failed_activity_update_leaves_every_registry_view_unchanged() {
    let route_key = key("/service");
    let route_data = route("http://127.0.0.1:9000/original");
    let store = Arc::new(FailingStore::with_routes(BTreeMap::from([(
        route_key.clone(),
        route_data,
    )])));
    let registry = RouteRegistry::load(store).await.unwrap();
    let before = registry_views(&registry, &route_key, "/service/request");

    let result = registry
        .update_activity(&route_key, Utc.timestamp_opt(999, 0).unwrap())
        .await;

    assert!(result.is_err());
    assert_eq!(
        registry_views(&registry, &route_key, "/service/request"),
        before
    );
}

#[tokio::test]
async fn failed_delete_leaves_every_registry_view_unchanged() {
    let route_key = key("/service");
    let route_data = route("http://127.0.0.1:9000/original");
    let store = Arc::new(FailingStore::with_routes(BTreeMap::from([(
        route_key.clone(),
        route_data,
    )])));
    let registry = RouteRegistry::load(store).await.unwrap();
    let before = registry_views(&registry, &route_key, "/service/request");

    let result = registry.delete(&route_key).await;

    assert!(result.is_err());
    assert_eq!(
        registry_views(&registry, &route_key, "/service/request"),
        before
    );
}

#[tokio::test]
async fn registry_loads_and_exposes_complete_store_snapshot() {
    let store = memory_store();
    let route_key = key("/user/a");
    let route_data = route("http://127.0.0.1:9000");
    store
        .put(route_key.clone(), route_data.clone())
        .await
        .unwrap();

    let registry = RouteRegistry::load(store).await.unwrap();

    assert_eq!(registry.get(&route_key), Some(route_data.clone()));
    assert_eq!(
        registry.all(),
        BTreeMap::from([(route_key.clone(), route_data)])
    );
    assert_eq!(registry.resolve("/user/a/tree").unwrap().key, route_key);
}

#[tokio::test]
async fn registry_overwrite_and_activity_update_publish_complete_records() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    let route_key = key("/service");
    registry
        .put(route_key.clone(), route("http://127.0.0.1:9000/original"))
        .await
        .unwrap();
    let replacement = route("http://127.0.0.1:9001/replacement");
    registry
        .put(route_key.clone(), replacement.clone())
        .await
        .unwrap();

    let activity = Utc.timestamp_opt(500, 0).unwrap();
    registry
        .update_activity(&route_key, activity)
        .await
        .unwrap();

    let actual = registry.get(&route_key).unwrap();
    assert_eq!(actual.target, replacement.target);
    assert_eq!(actual.extra, replacement.extra);
    assert_eq!(actual.last_activity, activity);
    assert_eq!(registry.all().len(), 1);
}

#[tokio::test]
async fn aliasing_route_keys_follow_successful_mutation_order_in_both_permutations() {
    for (first, second) in [("/service", "//service"), ("//service", "/service")] {
        let registry = RouteRegistry::load(memory_store()).await.unwrap();
        registry
            .put(key(first), route(&format!("http://first.example{first}")))
            .await
            .unwrap();
        registry
            .put(
                key(second),
                route(&format!("http://second.example{second}")),
            )
            .await
            .unwrap();

        let matched = registry.resolve("/service/request").unwrap();
        assert_eq!(
            matched.key,
            key(second),
            "mutation order {first:?}, {second:?}"
        );
    }
}

#[tokio::test]
async fn overwriting_an_alias_moves_it_to_the_end_of_matcher_order() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    registry
        .put(key("/service"), route("http://single.example/old"))
        .await
        .unwrap();
    registry
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();
    registry
        .put(key("/service"), route("http://single.example/new"))
        .await
        .unwrap();

    let matched = registry.resolve("/service/request").unwrap();
    assert_eq!(matched.key, key("/service"));
    assert_eq!(matched.data.target, "http://single.example/new");
}

#[tokio::test]
async fn deleting_the_winning_alias_restores_the_surviving_alias() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    registry
        .put(key("/service"), route("http://single.example"))
        .await
        .unwrap();
    registry
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();

    registry.delete(&key("//service")).await.unwrap();

    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        key("/service")
    );
}

#[tokio::test]
async fn activity_updates_do_not_change_alias_matcher_order() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    registry
        .put(key("/service"), route("http://single.example"))
        .await
        .unwrap();
    registry
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();

    registry
        .update_activity(&key("/service"), Utc.timestamp_opt(500, 0).unwrap())
        .await
        .unwrap();

    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        key("//service")
    );
}

#[tokio::test]
async fn initial_store_load_uses_deterministic_key_order_for_aliases() {
    let store = memory_store();
    store
        .put(key("/service"), route("http://single.example"))
        .await
        .unwrap();
    store
        .put(key("//service"), route("http://double.example"))
        .await
        .unwrap();

    let registry = RouteRegistry::load(store).await.unwrap();

    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        key("/service"),
        "initial load uses the snapshot's ascending RouteKey order"
    );
}

#[tokio::test]
async fn registry_delete_missing_is_a_noop_and_delete_returns_prior_route() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    let route_key = key("/service");
    let route_data = route("http://127.0.0.1:9000");
    registry
        .put(route_key.clone(), route_data.clone())
        .await
        .unwrap();

    assert_eq!(registry.delete(&key("/missing")).await.unwrap(), None);
    assert_eq!(registry.delete(&route_key).await.unwrap(), Some(route_data));
    assert!(registry.get(&route_key).is_none());
    assert!(registry.resolve("/service/tree").is_none());
}

#[derive(Clone, Copy)]
enum GatedManagementOperation {
    Add,
    Put,
    Delete,
}

struct GatedManagementStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    operation: GatedManagementOperation,
    entered: Semaphore,
    release: Semaphore,
}

impl GatedManagementStore {
    fn new(routes: BTreeMap<RouteKey, RouteData>, operation: GatedManagementOperation) -> Self {
        Self {
            routes: RwLock::new(routes),
            operation,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    async fn gate(&self) {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
    }
}

#[async_trait]
impl Store for GatedManagementStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        assert!(matches!(self.operation, GatedManagementOperation::Add));
        self.gate().await;
        let mut data = RouteData {
            target,
            last_activity: Utc.timestamp_opt(20, 0).unwrap(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        assert!(matches!(self.operation, GatedManagementOperation::Put));
        self.gate().await;
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        assert!(matches!(self.operation, GatedManagementOperation::Put));
        self.gate().await;
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
        assert!(matches!(self.operation, GatedManagementOperation::Delete));
        self.gate().await;
        Ok(self.routes.write().await.remove(key))
    }
}

async fn assert_blocked_management_preserves_other_route_activity(
    operation: GatedManagementOperation,
) {
    let activity_key = key("/active");
    let mutation_key = key("/mutated");
    let initial = BTreeMap::from([
        (activity_key.clone(), route("http://active.example")),
        (mutation_key.clone(), route("http://old.example")),
    ]);
    let store = Arc::new(GatedManagementStore::new(initial, operation));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    let mutation = {
        let registry = Arc::clone(&registry);
        let mutation_key = mutation_key.clone();
        tokio::spawn(async move {
            match operation {
                GatedManagementOperation::Add => registry
                    .add(mutation_key, "http://added.example".to_owned(), Map::new())
                    .await
                    .map(|_| ()),
                GatedManagementOperation::Put => {
                    registry
                        .put(mutation_key, route("http://put.example"))
                        .await
                }
                GatedManagementOperation::Delete => {
                    registry.delete(&mutation_key).await.map(|_| ())
                }
            }
        })
    };
    store.entered.acquire().await.unwrap().forget();

    let observed_at = Utc.timestamp_opt(30, 0).unwrap();
    assert!(registry.observe_activity(&activity_key, observed_at));
    assert_eq!(
        registry.get(&activity_key).unwrap().last_activity,
        observed_at
    );
    assert!(registry.observe_activity(&mutation_key, observed_at));

    store.release.add_permits(1);
    mutation.await.unwrap().unwrap();
    assert_eq!(
        registry.get(&activity_key).unwrap().last_activity,
        observed_at,
        "a persistence-first management publication must merge with the latest activity snapshot"
    );
    let persisted = store.snapshot().await.unwrap();
    let reloaded = RouteRegistry::load(store.clone()).await.unwrap();
    assert_eq!(reloaded.all(), persisted);
    if matches!(operation, GatedManagementOperation::Delete) {
        assert!(!persisted.contains_key(&mutation_key));
    } else {
        assert_eq!(persisted[&mutation_key].last_activity, observed_at);
        assert_eq!(
            registry.get(&mutation_key),
            Some(persisted[&mutation_key].clone())
        );
    }
}

#[tokio::test]
async fn blocked_add_merges_activity_observed_on_another_route() {
    assert_blocked_management_preserves_other_route_activity(GatedManagementOperation::Add).await;
}

#[tokio::test]
async fn blocked_put_merges_activity_observed_on_another_route() {
    assert_blocked_management_preserves_other_route_activity(GatedManagementOperation::Put).await;
}

#[tokio::test]
async fn blocked_delete_merges_activity_observed_on_another_route() {
    assert_blocked_management_preserves_other_route_activity(GatedManagementOperation::Delete)
        .await;
}

struct GatedPutStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    gates: BTreeMap<RouteKey, Arc<Semaphore>>,
    entered: Semaphore,
    entries: std::sync::Mutex<Vec<RouteKey>>,
}

impl GatedPutStore {
    fn new(route_keys: impl IntoIterator<Item = RouteKey>) -> Self {
        Self {
            routes: RwLock::new(BTreeMap::new()),
            gates: route_keys
                .into_iter()
                .map(|route_key| (route_key, Arc::new(Semaphore::new(0))))
                .collect(),
            entered: Semaphore::new(0),
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn release(&self, route_key: &RouteKey) {
        self.gates[route_key].add_permits(1);
    }

    fn entries(&self) -> Vec<RouteKey> {
        self.entries.lock().unwrap().clone()
    }
}

#[async_trait]
impl Store for GatedPutStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.entries.lock().unwrap().push(key.clone());
        self.entered.add_permits(1);
        self.gates[&key].acquire().await.unwrap().forget();
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.entries.lock().unwrap().push(key.clone());
        self.entered.add_permits(1);
        self.gates[&key].acquire().await.unwrap().forget();
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        unreachable!("the writer-serialization test only exercises put")
    }
}

#[tokio::test]
async fn concurrent_alias_writers_are_serialized_in_successful_mutation_order() {
    let first_key = key("/service");
    let second_key = key("//service");
    let store = Arc::new(GatedPutStore::new([first_key.clone(), second_key.clone()]));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let first_data = route("http://127.0.0.1:9001/first");
    let second_data = route("http://127.0.0.1:9002/second");

    let first = {
        let registry = Arc::clone(&registry);
        let first_key = first_key.clone();
        let first_data = first_data.clone();
        tokio::spawn(async move { registry.put(first_key, first_data).await })
    };
    store.entered.acquire().await.unwrap().forget();
    assert_eq!(store.entries(), vec![first_key.clone()]);
    let second = {
        let registry = Arc::clone(&registry);
        let second_key = second_key.clone();
        let second_data = second_data.clone();
        tokio::spawn(async move { registry.put(second_key, second_data).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(
        store.entries(),
        vec![first_key.clone()],
        "the second writer must not clone a stale snapshot or enter persistence"
    );

    store.release(&first_key);
    first.await.unwrap().unwrap();
    store.entered.acquire().await.unwrap().forget();
    assert_eq!(store.entries(), vec![first_key.clone(), second_key.clone()]);
    store.release(&second_key);
    second.await.unwrap().unwrap();

    let expected = BTreeMap::from([
        (first_key.clone(), first_data.clone()),
        (second_key.clone(), second_data.clone()),
    ]);
    assert_eq!(registry.all(), expected);
    assert_eq!(registry.get(&first_key), Some(first_data));
    assert_eq!(registry.get(&second_key), Some(second_data));
    assert_eq!(
        registry.resolve("/service/request").unwrap().key,
        second_key
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_observe_only_complete_snapshots() {
    let registry = RouteRegistry::load(memory_store()).await.unwrap();
    let route_key = key("/service");
    let old = route("http://old.example/generation-0");
    registry.put(route_key.clone(), old.clone()).await.unwrap();
    let start = Arc::new(Barrier::new(5));
    let old_observed = Arc::new(Barrier::new(5));
    let new_published = Arc::new(Barrier::new(5));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let registry = Arc::clone(&registry);
            let start = Arc::clone(&start);
            let old_observed = Arc::clone(&old_observed);
            let new_published = Arc::clone(&new_published);
            tokio::spawn(async move {
                start.wait().await;
                tokio::task::yield_now().await;
                let before = (*registry.resolve("/service/request").unwrap().data).clone();
                old_observed.wait().await;
                new_published.wait().await;
                tokio::task::yield_now().await;
                let after = (*registry.resolve("/service/request").unwrap().data).clone();
                (before, after)
            })
        })
        .collect();

    start.wait().await;
    old_observed.wait().await;
    tokio::task::yield_now().await;
    let mut replacement = route("http://new.example/generation-1");
    replacement.last_activity = Utc.timestamp_opt(2, 0).unwrap();
    registry.put(route_key, replacement.clone()).await.unwrap();
    tokio::task::yield_now().await;
    new_published.wait().await;

    for reader in readers {
        let (before, after) = reader.await.unwrap();
        assert_eq!(
            before, old,
            "reader did not observe the complete old snapshot"
        );
        assert_eq!(
            after, replacement,
            "reader did not observe the complete new snapshot"
        );
    }

    assert!(registry.resolve("\0/untrusted/runtime/path").is_none());
}

#[derive(Clone, Copy)]
enum SupervisedPutOutcome {
    Success,
    Error,
    Panic,
    PanicWithPanickingPayload,
}

struct PanickingPayloadDrop;

impl Drop for PanickingPayloadDrop {
    fn drop(&mut self) {
        panic!("PANICKING_PAYLOAD_DROP_SENTINEL_f6269624");
    }
}

struct SupervisedPutStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    entered: Semaphore,
    release: Semaphore,
    outcome: SupervisedPutOutcome,
}

impl SupervisedPutStore {
    fn new(outcome: SupervisedPutOutcome) -> Self {
        Self {
            routes: RwLock::new(BTreeMap::new()),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            outcome,
        }
    }
}

#[async_trait]
impl Store for SupervisedPutStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("unused add"))
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        match self.outcome {
            SupervisedPutOutcome::Success => {
                self.routes.write().await.insert(key, data);
                Ok(())
            }
            SupervisedPutOutcome::Error => Err(StoreError::message("detached backend failure")),
            SupervisedPutOutcome::Panic => {
                panic!("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e")
            }
            SupervisedPutOutcome::PanicWithPanickingPayload => {
                std::panic::panic_any(PanickingPayloadDrop)
            }
        }
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        match self.outcome {
            SupervisedPutOutcome::Success => {
                let mut routes = self.routes.write().await;
                if let Some(floor) = activity_floor.current() {
                    data.last_activity = data.last_activity.max(floor);
                }
                routes.insert(key, data.clone());
                Ok(data)
            }
            SupervisedPutOutcome::Error => Err(StoreError::message("detached backend failure")),
            SupervisedPutOutcome::Panic => {
                panic!("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e")
            }
            SupervisedPutOutcome::PanicWithPanickingPayload => {
                std::panic::panic_any(PanickingPayloadDrop)
            }
        }
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        Err(StoreError::message("unused activity update"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("unused delete"))
    }
}

#[test]
fn application_owned_panic_hook_delegates_unrelated_panics_and_redacts_mutations() {
    const CHILD_ENV: &str = "ROUTE_MUTATION_PANIC_HOOK_CHILD";
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "panic_hook_subprocess_child_receives_fixed_mutation_errors",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();

    assert!(output.status.success(), "child test failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("PREVIOUS_PANIC_HOOK_DELEGATED").count(), 1);
    assert!(stderr.contains("route mutation panic redacted at "));
    assert!(stderr.contains("tests/store_contract.rs:"));
    assert!(!stderr.contains("PREVIOUS_HOOK_SAW_MARKED_PANIC"));
    assert!(!stderr.contains("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e"));
    assert!(!stderr.contains("PANICKING_PAYLOAD_DROP_SENTINEL_f6269624"));
    assert!(!stderr.contains("UNRELATED_PANIC_SENTINEL_04bb8c85"));
}

#[test]
fn panic_hook_subprocess_child_receives_fixed_mutation_errors() {
    if std::env::var_os("ROUTE_MUTATION_PANIC_HOOK_CHILD").is_none() {
        return;
    }

    std::panic::set_hook(Box::new(|panic_info| {
        use std::io::Write as _;

        let unrelated = panic_info
            .payload()
            .downcast_ref::<&str>()
            .is_some_and(|payload| *payload == "UNRELATED_PANIC_SENTINEL_04bb8c85");
        let message = if unrelated {
            "PREVIOUS_PANIC_HOOK_DELEGATED"
        } else {
            "PREVIOUS_HOOK_SAW_MARKED_PANIC"
        };
        let _ = writeln!(std::io::stderr().lock(), "{message}");
    }));
    install_route_mutation_panic_hook_at_startup();

    let _ = std::panic::catch_unwind(|| panic!("UNRELATED_PANIC_SENTINEL_04bb8c85"));

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let live_store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
        let live_registry = RouteRegistry::load(live_store.clone()).await.unwrap();
        let live = tokio::spawn({
            let registry = Arc::clone(&live_registry);
            async move {
                registry
                    .put(key("/hook-live"), route("http://hook-live.example"))
                    .await
            }
        });
        live_store.entered.acquire().await.unwrap().forget();
        live_store.release.add_permits(1);
        assert_eq!(
            live.await.unwrap().unwrap_err().to_string(),
            "route put mutation task panicked"
        );
        let live_drain = live_registry.drain_mutations(Duration::from_secs(1)).await;
        assert!(!live_drain.timed_out);
        assert_eq!(live_drain.active_mutations, 0);
        assert!(live_drain.detached_panics.is_empty());

        let detached_store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
        let detached_registry = RouteRegistry::load(detached_store.clone()).await.unwrap();
        start_and_cancel_supervised_put(
            &detached_registry,
            &detached_store,
            key("/hook-detached"),
            route("http://hook-detached.example"),
        )
        .await;
        detached_store.release.add_permits(1);
        let drained = detached_registry
            .drain_mutations(Duration::from_secs(1))
            .await;
        assert!(!drained.timed_out);
        assert_eq!(drained.detached_panics.len(), 1);
        assert_eq!(
            drained.detached_panics[0].message,
            "route put mutation task panicked"
        );

        let drop_store = Arc::new(SupervisedPutStore::new(
            SupervisedPutOutcome::PanicWithPanickingPayload,
        ));
        let drop_registry = RouteRegistry::load(drop_store.clone()).await.unwrap();
        let drop_panic = tokio::spawn({
            let registry = Arc::clone(&drop_registry);
            async move {
                registry
                    .put(key("/hook-drop"), route("http://hook-drop.example"))
                    .await
            }
        });
        drop_store.entered.acquire().await.unwrap().forget();
        drop_store.release.add_permits(1);
        assert_eq!(
            drop_panic.await.unwrap().unwrap_err().to_string(),
            "route put mutation task panicked"
        );
    });
}

struct SnapshotFailureStore;

#[async_trait]
impl Store for SnapshotFailureStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Err(StoreError::message("injected snapshot failure"))
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!()
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        unreachable!()
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!()
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        unreachable!()
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        unreachable!()
    }
}

#[test]
fn registry_load_failure_does_not_install_or_replace_the_process_panic_hook() {
    const CHILD_ENV: &str = "ROUTE_LOAD_FAILURE_PANIC_HOOK_CHILD";
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "load_failure_subprocess_child_keeps_the_application_panic_hook",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();

    assert!(output.status.success(), "child test failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("LOAD_FAILURE_POST_HOOK_DELEGATED").count(),
        1
    );
    assert!(stderr.contains("route mutation panic redacted at "));
    assert!(!stderr.contains("LOAD_FAILURE_HOOK_SAW_MARKED_PANIC"));
    assert!(!stderr.contains("LOAD_FAILURE_UNRELATED_SENTINEL_109cbc43"));
    assert!(!stderr.contains("SUPERVISOR_PANIC_SECRET_SENTINEL_7d69f58e"));
}

#[test]
fn load_failure_subprocess_child_keeps_the_application_panic_hook() {
    if std::env::var_os("ROUTE_LOAD_FAILURE_PANIC_HOOK_CHILD").is_none() {
        return;
    }

    std::panic::set_hook(Box::new(|_| {}));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let Err(error) = runtime.block_on(RouteRegistry::load(Arc::new(SnapshotFailureStore))) else {
        panic!("snapshot failure must reject registry load");
    };
    assert_eq!(error.to_string(), "injected snapshot failure");

    drop(std::panic::take_hook());
    std::panic::set_hook(Box::new(|panic_info| {
        use std::io::Write as _;

        let unrelated = panic_info
            .payload()
            .downcast_ref::<&str>()
            .is_some_and(|payload| *payload == "LOAD_FAILURE_UNRELATED_SENTINEL_109cbc43");
        let message = if unrelated {
            "LOAD_FAILURE_POST_HOOK_DELEGATED"
        } else {
            "LOAD_FAILURE_HOOK_SAW_MARKED_PANIC"
        };
        let _ = writeln!(std::io::stderr().lock(), "{message}");
    }));
    install_route_mutation_panic_hook_at_startup();
    let _ = std::panic::catch_unwind(|| panic!("LOAD_FAILURE_UNRELATED_SENTINEL_109cbc43"));
    runtime.block_on(async {
        let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Panic));
        let registry = RouteRegistry::load(store.clone()).await.unwrap();
        let mutation = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .put(key("/load-hook"), route("http://load-hook.example"))
                    .await
            }
        });
        store.entered.acquire().await.unwrap().forget();
        store.release.add_permits(1);
        assert_eq!(
            mutation.await.unwrap().unwrap_err().to_string(),
            "route put mutation task panicked"
        );
    });
}

async fn start_and_cancel_supervised_put(
    registry: &Arc<RouteRegistry>,
    store: &Arc<SupervisedPutStore>,
    route_key: RouteKey,
    route_data: RouteData,
) {
    let caller = {
        let registry = Arc::clone(registry);
        tokio::spawn(async move { registry.put(route_key, route_data).await })
    };
    store.entered.acquire().await.unwrap().forget();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
}

#[derive(Default)]
struct AdmissionCountingStore {
    mutation_calls: AtomicUsize,
}

#[async_trait]
impl Store for AdmissionCountingStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(BTreeMap::new())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(route("http://unexpected-add.example"))
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(data)
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

struct FirstMutationPanicStore {
    mutation_calls: AtomicUsize,
    first_entered: Semaphore,
    release_first: Semaphore,
}

impl FirstMutationPanicStore {
    fn new() -> Self {
        Self {
            mutation_calls: AtomicUsize::new(0),
            first_entered: Semaphore::new(0),
            release_first: Semaphore::new(0),
        }
    }

    fn enter(&self) -> bool {
        self.mutation_calls.fetch_add(1, Ordering::SeqCst) == 0
    }
}

#[async_trait]
impl Store for FirstMutationPanicStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(BTreeMap::new())
    }

    async fn add(
        &self,
        _key: RouteKey,
        target: String,
        extra: Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        assert!(!self.enter(), "the first store mutation must be put");
        let mut data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        Ok(data)
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        if self.enter() {
            self.first_entered.add_permits(1);
            self.release_first.acquire().await.unwrap().forget();
            std::panic::panic_any(PanickingPayloadDrop);
        }
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        if self.enter() {
            self.first_entered.add_permits(1);
            self.release_first.acquire().await.unwrap().forget();
            std::panic::panic_any(PanickingPayloadDrop);
        }
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        Ok(data)
    }

    async fn update_activity(&self, _key: &RouteKey, _at: DateTime<Utc>) -> Result<(), StoreError> {
        assert!(!self.enter(), "the first store mutation must be put");
        Ok(())
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        assert!(!self.enter(), "the first store mutation must be put");
        Ok(None)
    }
}

#[tokio::test]
async fn first_backend_panic_terminally_seals_queued_and_later_mutations() {
    let store = Arc::new(FirstMutationPanicStore::new());
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let first = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .put(key("/panic/first"), route("http://panic-first.example"))
                .await
                .map(|_| ())
        }
    });
    store.first_entered.acquire().await.unwrap().forget();

    let queued = [
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .add(
                        key("/panic/queued-add"),
                        "http://queued-add.example".to_owned(),
                        Map::new(),
                    )
                    .await
                    .map(|_| ())
            }
        }),
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .put(key("/panic/queued-put"), route("http://queued-put.example"))
                    .await
                    .map(|_| ())
            }
        }),
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .update_activity(&key("/panic/queued-activity"), Utc::now())
                    .await
                    .map(|_| ())
            }
        }),
        tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .delete(&key("/panic/queued-delete"))
                    .await
                    .map(|_| ())
            }
        }),
    ];

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if registry.mutation_status().active_mutations == 5 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    store.release_first.add_permits(1);

    assert_eq!(
        first.await.unwrap().unwrap_err().to_string(),
        "route put mutation task panicked"
    );
    for mutation in queued {
        assert_eq!(
            mutation.await.unwrap().unwrap_err().to_string(),
            MUTATION_PANIC_SEALED_ERROR
        );
    }

    let status = registry.mutation_status();
    assert!(status.sealed);
    assert_eq!(status.seal, MutationSeal::BackendPanic);
    assert_eq!(status.active_mutations, 0);

    let later_key = key("/panic/later");
    let later_errors = [
        registry
            .add(
                later_key.clone(),
                "http://later-add.example".to_owned(),
                Map::new(),
            )
            .await
            .unwrap_err(),
        registry
            .put(later_key.clone(), route("http://later-put.example"))
            .await
            .unwrap_err(),
        registry
            .update_activity(&later_key, Utc::now())
            .await
            .unwrap_err(),
        registry.delete(&later_key).await.unwrap_err(),
    ];
    assert!(later_errors
        .iter()
        .all(|error| error.to_string() == MUTATION_PANIC_SEALED_ERROR));

    let drained = registry.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(drained.detached_failures.is_empty());
    assert!(drained.detached_panics.is_empty());
    assert_eq!(registry.mutation_status().seal, MutationSeal::BackendPanic);
    assert_eq!(store.mutation_calls.load(Ordering::SeqCst), 1);
    assert!(registry.all().is_empty());
}

#[tokio::test]
async fn seal_wins_before_begin_and_rejects_every_mutation_without_store_or_publication() {
    let store = Arc::new(AdmissionCountingStore::default());
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    let drained = registry.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    let status = registry.mutation_status();
    assert!(status.sealed);
    assert_eq!(status.active_mutations, 0);

    let route_key = key("/sealed");
    let errors = [
        registry
            .add(
                route_key.clone(),
                "http://sealed-add.example".to_owned(),
                Map::new(),
            )
            .await
            .unwrap_err(),
        registry
            .put(route_key.clone(), route("http://sealed-put.example"))
            .await
            .unwrap_err(),
        registry
            .update_activity(&route_key, Utc::now())
            .await
            .unwrap_err(),
        registry.delete(&route_key).await.unwrap_err(),
    ];
    assert!(errors
        .iter()
        .all(|error| error.to_string() == MUTATION_ADMISSION_SEALED_ERROR));
    assert_eq!(store.mutation_calls.load(Ordering::SeqCst), 0);
    assert!(registry.all().is_empty());
}

#[tokio::test]
async fn begin_wins_before_seal_then_drains_while_later_mutations_are_rejected() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let begun = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .put(key("/begun"), route("http://begun.example"))
                .await
        }
    });
    store.entered.acquire().await.unwrap().forget();
    let before_seal = registry.mutation_status();
    assert!(!before_seal.sealed);
    assert_eq!(before_seal.active_mutations, 1);

    let draining = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.drain_mutations(Duration::from_secs(1)).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if registry.mutation_status().sealed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rejected = registry
        .put(key("/late"), route("http://late.example"))
        .await
        .unwrap_err();
    assert_eq!(rejected.to_string(), MUTATION_ADMISSION_SEALED_ERROR);

    store.release.add_permits(1);
    begun.await.unwrap().unwrap();
    let drained = draining.await.unwrap();
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(registry.get(&key("/begun")).is_some());
    assert!(registry.get(&key("/late")).is_none());
}

#[tokio::test]
async fn accepted_but_unpolled_handler_is_rejected_after_drain_seals_admission() {
    let store = Arc::new(AdmissionCountingStore::default());
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let accepted = registry.put(key("/accepted"), route("http://accepted.example"));

    let drained = registry.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    let error = accepted.await.unwrap_err();
    assert_eq!(error.to_string(), MUTATION_ADMISSION_SEALED_ERROR);
    assert_eq!(store.mutation_calls.load(Ordering::SeqCst), 0);
    assert!(registry.get(&key("/accepted")).is_none());
}

#[tokio::test]
async fn cancelled_caller_later_success_converges_and_drain_succeeds() {
    let route_key = key("/supervised-success");
    let route_data = route("http://success.example");
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    start_and_cancel_supervised_put(&registry, &store, route_key.clone(), route_data.clone()).await;
    store.release.add_permits(1);

    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(drained.detached_failures.is_empty());
    assert!(drained.detached_panics.is_empty());
    assert_eq!(registry.get(&route_key), Some(route_data));
}

#[tokio::test]
async fn cancelled_caller_later_store_error_is_surfaced_by_drain() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Error));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();

    start_and_cancel_supervised_put(
        &registry,
        &store,
        key("/supervised-error"),
        route("http://error.example"),
    )
    .await;
    let while_active = registry.drain_mutations(Duration::ZERO).await;
    assert!(while_active.timed_out);
    assert_eq!(while_active.active_mutations, 1);
    assert!(while_active.detached_failures.is_empty());
    store.release.add_permits(1);

    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert_eq!(drained.detached_failures.len(), 1);
    assert_eq!(
        drained.detached_failures[0].operation,
        MutationOperation::Put
    );
    assert_eq!(
        drained.detached_failures[0].error.to_string(),
        "detached backend failure"
    );
    assert!(!format!("{drained:?}").contains("detached backend failure"));
    assert!(drained.detached_panics.is_empty());
}

#[tokio::test]
async fn drain_times_out_while_backend_pending_then_succeeds_after_release() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    start_and_cancel_supervised_put(
        &registry,
        &store,
        key("/pending"),
        route("http://pending.example"),
    )
    .await;

    let timed_out = registry.drain_mutations(Duration::from_millis(10)).await;
    assert!(timed_out.timed_out);
    assert_eq!(timed_out.active_mutations, 1);

    store.release.add_permits(1);
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
}

#[tokio::test]
async fn zero_duration_drain_distinguishes_inactive_from_active() {
    let inactive = RouteRegistry::load(memory_store()).await.unwrap();
    let drained = inactive.drain_mutations(Duration::ZERO).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);

    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let active = RouteRegistry::load(store.clone()).await.unwrap();
    start_and_cancel_supervised_put(
        &active,
        &store,
        key("/zero-active"),
        route("http://zero-active.example"),
    )
    .await;

    let timed_out = active.drain_mutations(Duration::ZERO).await;
    assert!(timed_out.timed_out);
    assert_eq!(timed_out.active_mutations, 1);

    store.release.add_permits(1);
    assert!(
        !active
            .drain_mutations(Duration::from_secs(1))
            .await
            .timed_out
    );
}

#[tokio::test]
async fn completion_at_drain_deadline_never_reports_timeout_with_zero_active() {
    for index in 0..128 {
        let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
        let registry = RouteRegistry::load(store.clone()).await.unwrap();
        start_and_cancel_supervised_put(
            &registry,
            &store,
            key(&format!("/deadline/{index}")),
            route("http://deadline.example"),
        )
        .await;

        let release = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                tokio::task::yield_now().await;
                store.release.add_permits(1);
            }
        });
        let outcome = registry.drain_mutations(Duration::ZERO).await;
        assert!(!(outcome.timed_out && outcome.active_mutations == 0));
        release.await.unwrap();
        if outcome.active_mutations != 0 {
            let completed = registry.drain_mutations(Duration::from_secs(1)).await;
            assert!(!completed.timed_out);
            assert_eq!(completed.active_mutations, 0);
        }
    }
}

#[tokio::test]
async fn detached_diagnostics_are_bounded_and_consumed_once() {
    let total = DETACHED_MUTATION_DIAGNOSTIC_CAPACITY + 37;
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Error));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let callers: Vec<_> = (0..total)
        .map(|index| {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                registry
                    .put(
                        key(&format!("/overflow/{index}")),
                        route("http://overflow.example"),
                    )
                    .await
            })
        })
        .collect();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = registry.mutation_status();
            if status.active_mutations == total {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for caller in &callers {
        caller.abort();
    }
    for caller in callers {
        assert!(caller.await.unwrap_err().is_cancelled());
    }
    store.release.add_permits(total);

    let drained = registry.drain_mutations(Duration::from_secs(5)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert_eq!(
        drained.detached_failures.len() + drained.detached_panics.len(),
        DETACHED_MUTATION_DIAGNOSTIC_CAPACITY
    );
    assert_eq!(
        drained.dropped_detached_failures,
        total - DETACHED_MUTATION_DIAGNOSTIC_CAPACITY
    );
    assert_eq!(drained.dropped_detached_panics, 0);

    let consumed = registry.drain_mutations(Duration::ZERO).await;
    assert!(!consumed.timed_out);
    assert!(consumed.detached_failures.is_empty());
    assert!(consumed.detached_panics.is_empty());
    assert_eq!(consumed.dropped_detached_failures, 0);
    assert_eq!(consumed.dropped_detached_panics, 0);
    assert!(registry.mutation_status().sealed);
    assert_eq!(
        registry
            .put(key("/after-repeat"), route("http://after-repeat.example"))
            .await
            .unwrap_err()
            .to_string(),
        MUTATION_ADMISSION_SEALED_ERROR
    );
}

#[tokio::test]
async fn multiple_concurrent_mutations_drain_and_registry_lifetime_is_released() {
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let lifetime = Arc::downgrade(&registry);
    let callers: Vec<_> = (0..16)
        .map(|index| {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                registry
                    .put(
                        key(&format!("/concurrent/{index}")),
                        route(&format!("http://concurrent-{index}.example")),
                    )
                    .await
            })
        })
        .collect();

    store.entered.acquire().await.unwrap().forget();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if registry.mutation_status().active_mutations == 16 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let draining = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.drain_mutations(Duration::from_secs(1)).await })
    };
    store.release.add_permits(16);
    let drained = draining.await.unwrap();
    assert!(!drained.timed_out);
    for caller in callers {
        caller.await.unwrap().unwrap();
    }
    assert_eq!(registry.all().len(), 16);

    drop(registry);
    assert!(lifetime.upgrade().is_none());
}

#[test]
fn mutation_without_a_runtime_fails_and_leaves_the_tracker_inactive() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime
        .block_on(RouteRegistry::load(memory_store()))
        .unwrap();
    drop(runtime);

    let mut mutation =
        Box::pin(registry.put(key("/no-runtime"), route("http://no-runtime.example")));
    let mut context = Context::from_waker(Waker::noop());
    let Poll::Ready(result) = mutation.as_mut().poll(&mut context) else {
        panic!("missing-runtime failure must be immediate");
    };
    assert_eq!(
        result.unwrap_err().to_string(),
        "route put mutation could not start: Tokio runtime unavailable"
    );
    drop(mutation);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let drained = runtime.block_on(registry.drain_mutations(Duration::from_millis(10)));
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert!(drained.detached_failures.is_empty());
    assert!(drained.detached_panics.is_empty());
}

#[test]
fn terminal_seal_takes_precedence_over_runtime_availability() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime
        .block_on(RouteRegistry::load(memory_store()))
        .unwrap();
    let drained = runtime.block_on(registry.drain_mutations(Duration::ZERO));
    assert!(!drained.timed_out);
    drop(runtime);

    let mut mutation =
        Box::pin(registry.put(key("/sealed-no-runtime"), route("http://sealed.example")));
    let mut context = Context::from_waker(Waker::noop());
    let Poll::Ready(result) = mutation.as_mut().poll(&mut context) else {
        panic!("sealed mutation rejection must be immediate");
    };
    assert_eq!(
        result.unwrap_err().to_string(),
        MUTATION_ADMISSION_SEALED_ERROR
    );
}

#[test]
fn runtime_shutdown_does_not_leak_active_count_or_registry_lifetime() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = Arc::new(SupervisedPutStore::new(SupervisedPutOutcome::Success));
    let registry = runtime
        .block_on(RouteRegistry::load(store.clone()))
        .unwrap();
    let lifetime = Arc::downgrade(&registry);
    let caller = runtime.spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .put(key("/runtime-shutdown"), route("http://shutdown.example"))
                .await
        }
    });
    runtime.block_on(async { store.entered.acquire().await.unwrap().forget() });

    drop(runtime);
    drop(caller);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let drained = runtime.block_on(registry.drain_mutations(Duration::from_secs(1)));
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert_eq!(drained.detached_panics.len(), 1);
    assert_eq!(drained.detached_panics[0].operation, MutationOperation::Put);
    assert_eq!(
        drained.detached_panics[0].message,
        "route put mutation task terminated unexpectedly"
    );

    drop(registry);
    assert!(lifetime.upgrade().is_none());
}
