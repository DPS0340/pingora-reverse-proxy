mod support;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use pingora_reverse_proxy::store::{ActivityFloor, Store, StoreError};
use proptest::prelude::*;
use serde_json::{json, Value};
use tokio::sync::{RwLock, Semaphore};

use pingora_reverse_proxy::route::{RouteData, RouteKey};
use support::*;

const TARGET: &str = "http://127.0.0.1:9000/base";

async fn post_route(app: &TestApi, path: &str, target: &str) -> axum::response::Response {
    request(
        app,
        "POST",
        path,
        Some(json!({ "target": target })),
        app.authorization().as_deref(),
    )
    .await
}

#[tokio::test]
async fn configured_token_is_required_and_rejections_are_empty_403s() {
    let app = test_api(Some("goodtoken")).await;

    for auth in [
        None,
        Some("token wrong"),
        Some("Token goodtoken"),
        Some("token goodtokenX trailing"),
    ] {
        let response = request(&app, "GET", "/api/routes", None, auth).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(read_bytes(response).await.is_empty());
    }
}

#[tokio::test]
async fn token_parser_matches_chp_case_sensitive_unanchored_semantics() {
    let app = test_api(Some("tok")).await;

    for auth in [
        "token tok",
        "token tok trailing",
        "prefix token tok",
        "prefix token\t tok suffix",
    ] {
        let response = request(&app, "GET", "/api/routes", None, Some(auth)).await;
        assert_eq!(response.status(), StatusCode::OK, "auth header {auth:?}");
    }
}

#[tokio::test]
async fn no_configured_token_allows_requests_without_authorization() {
    let app = test_api(None).await;
    assert_eq!(
        request(&app, "GET", "/api/routes", None, None)
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn get_all_returns_the_complete_route_table_and_iso_timestamp() {
    let app = test_api(None).await;
    assert_eq!(
        post_route(&app, "/api/routes/user/alice", TARGET)
            .await
            .status(),
        StatusCode::CREATED
    );

    let routes = get_json(&app, "/api/routes").await;
    assert_eq!(routes["/user/alice"]["target"], TARGET);
    assert!(routes["/user/alice"]["last_activity"]
        .as_str()
        .is_some_and(|timestamp| timestamp.ends_with('Z')));
    let timestamp = routes["/user/alice"]["last_activity"].as_str().unwrap();
    assert_eq!(
        timestamp.split_once('.').unwrap().1.len(),
        4,
        "CHP always emits exactly three fractional digits followed by Z: {timestamp}"
    );
}

#[tokio::test]
async fn get_one_returns_route_data_and_missing_route_is_empty_404() {
    let app = test_api(None).await;
    post_route(&app, "/api/routes/user/alice", TARGET).await;

    let route = get_json(&app, "/api/routes/user/alice").await;
    assert_eq!(route["target"], TARGET);

    let response = request(&app, "GET", "/api/routes/missing", None, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(read_bytes(response).await.is_empty());
}

#[tokio::test]
async fn post_root_path_and_escaped_route_return_empty_201s() {
    let app = test_api(None).await;
    for path in [
        "/api/routes",
        "/api/routes/user/alice",
        "/api/routes/user/foo%40bar",
    ] {
        let response = post_route(&app, path, TARGET).await;
        assert_eq!(response.status(), StatusCode::CREATED, "POST {path}");
        assert!(read_bytes(response).await.is_empty());
    }

    let routes = get_json(&app, "/api/routes").await;
    assert!(routes.get("/").is_some());
    assert!(routes.get("/user/alice").is_some());
    assert!(routes.get("/user/foo@bar").is_some());
}

#[tokio::test]
async fn post_preserves_unknown_jupyterhub_metadata() {
    let app = test_api(Some("secret")).await;
    let response = request(
        &app,
        "POST",
        "/api/routes/user/%E7%A7%80%E6%A8%B9",
        Some(json!({
            "target": TARGET,
            "jupyterhub": true,
            "user": "秀樹",
            "nested": { "server": "lab", "options": [1, null, false] }
        })),
        Some("token secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let route = get_json(&app, "/api/routes/user/%E7%A7%80%E6%A8%B9").await;
    assert_eq!(route["user"], "秀樹");
    assert_eq!(route["jupyterhub"], true);
    assert_eq!(
        route["nested"],
        json!({ "server": "lab", "options": [1, null, false] })
    );
}

#[tokio::test]
async fn malformed_post_bodies_match_chp_errors_and_do_not_publish() {
    let app = test_api(None).await;

    for body in [json!({}), json!({ "target": null }), json!({ "target": 5 })] {
        let response = request(&app, "POST", "/api/routes/rejected", Some(body), None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(read_text(response).await, "Must specify 'target' as string");
    }

    let pinned_v8_errors = [
        ("", "SyntaxError: Unexpected end of JSON input"),
        (
            "{",
            "SyntaxError: Expected property name or '}' in JSON at position 1 (line 1 column 2)",
        ),
        (
            r#"{"target":"x",}"#,
            "SyntaxError: Expected double-quoted property name in JSON at position 14 (line 1 column 15)",
        ),
        (
            "[1,]",
            "SyntaxError: Unexpected token ']', \"[1,]\" is not valid JSON",
        ),
        (
            "{ definitely not json",
            "SyntaxError: Expected property name or '}' in JSON at position 2 (line 1 column 3)",
        ),
        (
            "null trailing",
            "SyntaxError: Unexpected non-whitespace character after JSON at position 5 (line 1 column 6)",
        ),
        (
            "\"unterminated",
            "SyntaxError: Unterminated string in JSON at position 13 (line 1 column 14)",
        ),
    ];
    for (body, oracle_error) in pinned_v8_errors {
        let response = request_raw(
            &app,
            "POST",
            "/api/routes/rejected",
            Some(body.as_bytes().to_vec()),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body:?}");
        assert_eq!(
            read_text(response).await,
            format!("Body not valid JSON: {oracle_error}"),
            "{body:?}"
        );
    }
    assert_eq!(get_json(&app, "/api/routes").await, json!({}));
}

#[tokio::test]
async fn delete_returns_empty_204_or_empty_404() {
    let app = test_api(None).await;
    post_route(&app, "/api/routes/user/alice", TARGET).await;

    let deleted = request(&app, "DELETE", "/api/routes/user/alice", None, None).await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(read_bytes(deleted).await.is_empty());

    let missing = request(&app, "DELETE", "/api/routes/user/alice", None, None).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(read_bytes(missing).await.is_empty());
}

#[tokio::test]
async fn method_and_path_errors_match_chp_bodies() {
    let app = test_api(None).await;
    let method = request(&app, "PUT", "/api/routes", None, None).await;
    assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(read_text(method).await, "Method not supported");

    let path = request(&app, "GET", "/api/unknown", None, None).await;
    assert_eq!(path.status(), StatusCode::NOT_FOUND);
    assert_eq!(read_text(path).await, "Not Found");
}

#[tokio::test]
async fn head_is_an_explicit_405_instead_of_axums_automatic_get() {
    let app = test_api(None).await;

    for path in ["/api/routes", "/api/routes/missing"] {
        let response = request(&app, "HEAD", path, None, None).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert!(read_bytes(response).await.is_empty(), "{path}");
    }
}

#[tokio::test]
async fn post_parses_json_and_enforces_the_body_limit_before_authentication() {
    let app = test_api(Some("secret")).await;

    let malformed = request_raw(
        &app,
        "POST",
        "/api/routes/rejected",
        Some(b"{".to_vec()),
        Some("token wrong"),
    )
    .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_text(malformed).await,
        "Body not valid JSON: SyntaxError: Expected property name or '}' in JSON at position 1 (line 1 column 2)"
    );

    let prefix = br#"{"target":""#;
    let suffix = br#""}"#;
    let mut at_limit = Vec::with_capacity(1024 * 1024);
    at_limit.extend_from_slice(prefix);
    at_limit.resize(1024 * 1024 - suffix.len(), b'x');
    at_limit.extend_from_slice(suffix);
    assert_eq!(at_limit.len(), 1024 * 1024);

    let accepted_size = request_raw(
        &app,
        "POST",
        "/api/routes/large",
        Some(at_limit.clone()),
        Some("token wrong"),
    )
    .await;
    assert_eq!(accepted_size.status(), StatusCode::FORBIDDEN);

    at_limit.push(b' ');
    let too_large = request_raw(
        &app,
        "POST",
        "/api/routes/large",
        Some(at_limit),
        Some("token wrong"),
    )
    .await;
    assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let snapshot = app.metrics.snapshot();
    assert_eq!(snapshot.requests_api.get(&400), Some(&1));
    assert_eq!(snapshot.requests_api.get(&403), Some(&1));
    assert_eq!(snapshot.requests_api.get(&413), Some(&1));
}

#[tokio::test]
async fn every_string_target_is_accepted_and_round_trips_verbatim() {
    let app = test_api(None).await;
    let targets = [
        "",
        "not a URL",
        ":::",
        "HTTP://Example.COM:80/a/../b?x=%2f",
        "unix+http://%2Ftmp%2Fsocket",
    ];

    for (index, target) in targets.into_iter().enumerate() {
        let path = format!("/api/routes/target/{index}");
        let response = post_route(&app, &path, target).await;
        assert_eq!(response.status(), StatusCode::CREATED, "{target:?}");
        assert_eq!(get_json(&app, &path).await["target"], target);
    }
}

#[tokio::test]
async fn inactive_since_accepts_the_pinned_timezone_stable_date_parse_subset() {
    // Captured from Date.parse under the pinned CHP runtime. Every accepted
    // form has timezone-independent semantics.
    let accepted = [
        ("2020-01-01", "2020-01-01T00:00:00.000Z"),
        ("2020-01-01T12:34:56.789Z", "2020-01-01T12:34:56.789Z"),
        ("2020-01-01T12:34:56+09:00", "2020-01-01T03:34:56.000Z"),
        ("Wed, 01 Jan 2020 00:00:00 GMT", "2020-01-01T00:00:00.000Z"),
        ("2020-01-01 00:00:00Z", "2020-01-01T00:00:00.000Z"),
    ];
    for (input, oracle_iso) in accepted {
        let app = test_api(None).await;
        let boundary = chrono::DateTime::parse_from_rfc3339(oracle_iso)
            .unwrap()
            .with_timezone(&Utc);
        for (key, last_activity) in [
            ("/before", boundary - chrono::Duration::milliseconds(1)),
            ("/boundary", boundary),
        ] {
            app.registry
                .put(
                    RouteKey::parse(key).unwrap(),
                    RouteData {
                        target: TARGET.to_owned(),
                        last_activity,
                        extra: serde_json::Map::new(),
                    },
                )
                .await
                .unwrap();
        }

        let encoded: String = url::form_urlencoded::byte_serialize(input.as_bytes()).collect();
        let response = request(
            &app,
            "GET",
            &format!("/api/routes?inactiveSince={encoded}"),
            None,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "Date.parse({input:?})");
        let filtered = read_json(response).await;
        assert!(filtered.get("/before").is_some(), "{input:?}: {filtered}");
        assert!(filtered.get("/boundary").is_none(), "{input:?}: {filtered}");
    }
}

#[tokio::test]
async fn inactive_since_rejects_locale_and_local_time_dependent_date_forms() {
    let app = test_api(None).await;

    for input in ["January 1, 2020", "01/02/2020", "2020-01-01T00:00:00"] {
        let encoded: String = url::form_urlencoded::byte_serialize(input.as_bytes()).collect();
        let response = request(
            &app,
            "GET",
            &format!("/api/routes?inactiveSince={encoded}"),
            None,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{input:?}");
    }
}

#[tokio::test]
async fn route_methods_apply_whatwg_pathname_dot_segments_before_decoding() {
    // Pinned Node WHATWG URL pathname oracle fixtures, sent through Request URI
    // construction without client-side path normalization.
    let fixtures = [
        ("/api/routes/a/../b", "/api/routes/b"),
        ("/api/routes/a/%2e%2e/b", "/api/routes/b"),
        ("/api/routes/a/%2E/b", "/api/routes/a/b"),
    ];

    for (path_as_is, normalized) in fixtures {
        let app = test_api(None).await;
        assert_eq!(
            post_route(&app, path_as_is, path_as_is).await.status(),
            StatusCode::CREATED,
            "POST {path_as_is}"
        );
        assert_eq!(
            get_json(&app, path_as_is).await["target"],
            path_as_is,
            "GET {path_as_is}"
        );
        assert_eq!(get_json(&app, normalized).await["target"], path_as_is);
        assert_eq!(
            request(&app, "DELETE", path_as_is, None, None)
                .await
                .status(),
            StatusCode::NO_CONTENT,
            "DELETE {path_as_is}"
        );
        assert_eq!(
            request(&app, "GET", normalized, None, None).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn api_route_cleaning_adds_and_removes_at_most_one_outer_slash() {
    let app = test_api(None).await;
    let fixtures = [
        ("/api/routes//double//", "//double"),
        ("/api/routes/%2Fescaped%2F", "//escaped"),
        ("/api/routes///triple///", "///triple/"),
    ];

    for (path, _) in fixtures {
        assert_eq!(
            post_route(&app, path, "x").await.status(),
            StatusCode::CREATED
        );
    }

    let routes = get_json(&app, "/api/routes").await;
    for (_, expected_key) in fixtures {
        assert!(
            routes.get(expected_key).is_some(),
            "missing {expected_key:?}: {routes}"
        );
    }
}

#[tokio::test]
async fn delete_uses_chps_single_lookup_cleaning_pass() {
    let app = test_api(None).await;
    let path = "/api/routes/delete-double//";

    assert_eq!(
        post_route(&app, path, "x").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        request(&app, "DELETE", path, None, None).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_json(&app, path).await["target"], "x");
}

#[tokio::test]
async fn empty_configured_token_disables_auth_and_nbsp_is_javascript_whitespace() {
    let disabled = test_api(Some("")).await;
    assert_eq!(
        request(&disabled, "GET", "/api/routes", None, None)
            .await
            .status(),
        StatusCode::OK
    );

    let protected = test_api(Some("secret")).await;
    for authorization in ["token\u{00a0}secret", "prefix token\u{00a0}secret trailing"] {
        assert_eq!(
            request(&protected, "GET", "/api/routes", None, Some(authorization))
                .await
                .status(),
            StatusCode::OK,
            "{authorization:?}"
        );
    }
}

struct GatedAtomicAddStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    entered: Semaphore,
    release: Semaphore,
}

struct CommitThenReturnAddStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
    committed: Semaphore,
    return_response: Semaphore,
}

impl CommitThenReturnAddStore {
    fn new() -> Self {
        Self {
            routes: RwLock::new(BTreeMap::new()),
            committed: Semaphore::new(0),
            return_response: Semaphore::new(0),
        }
    }
}

#[async_trait]
impl Store for CommitThenReturnAddStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: serde_json::Map<String, Value>,
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
        self.committed.add_permits(1);
        self.return_response
            .acquire()
            .await
            .expect("test gate open")
            .forget();
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

    async fn update_activity(
        &self,
        key: &RouteKey,
        at: chrono::DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if let Some(route) = self.routes.write().await.get_mut(key) {
            route.last_activity = at;
        }
        Ok(())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Ok(self.routes.write().await.remove(key))
    }
}

impl GatedAtomicAddStore {
    fn new() -> Self {
        Self {
            routes: RwLock::new(BTreeMap::new()),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
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
        extra: serde_json::Map<String, Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("test gate open")
            .forget();
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

    async fn update_activity(
        &self,
        key: &RouteKey,
        at: chrono::DateTime<Utc>,
    ) -> Result<(), StoreError> {
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
async fn post_publishes_the_timestamp_returned_after_slow_atomic_persistence() {
    let store = Arc::new(GatedAtomicAddStore::new());
    let app = test_api_with_store(None, store.clone()).await;
    let request_app = app.clone();
    let posting =
        tokio::spawn(async move { post_route(&request_app, "/api/routes/slow", TARGET).await });

    store.entered.acquire().await.unwrap().forget();
    let persistence_finished_after = Utc::now();
    store.release.add_permits(1);
    assert_eq!(posting.await.unwrap().status(), StatusCode::CREATED);

    let route = app
        .registry
        .get(&RouteKey::parse("/slow").unwrap())
        .unwrap();
    assert!(
        route.last_activity >= persistence_finished_after,
        "last_activity={} was captured before persistence completed at {}",
        route.last_activity,
        persistence_finished_after
    );
}

#[tokio::test]
async fn cancelling_post_after_store_commit_does_not_cancel_registry_publication() {
    let route_key = RouteKey::parse("/committed").unwrap();
    let missing_key = RouteKey::parse("/missing").unwrap();
    let store = Arc::new(CommitThenReturnAddStore::new());
    let app = test_api_with_store(None, store.clone()).await;
    let request_app = app.clone();
    let posting =
        tokio::spawn(
            async move { post_route(&request_app, "/api/routes/committed", TARGET).await },
        );

    store.committed.acquire().await.unwrap().forget();
    posting.abort();
    assert!(posting.await.unwrap_err().is_cancelled());
    assert!(app.registry.get(&route_key).is_none());

    let mut queued_mutation = Box::pin(app.registry.delete(&missing_key));
    assert!(
        poll_once(queued_mutation.as_mut()).is_pending(),
        "the registry-owned add must retain the mutation lock after caller cancellation"
    );
    store.return_response.add_permits(1);
    assert_eq!(queued_mutation.await.unwrap(), None);

    let committed = store.snapshot().await.unwrap().remove(&route_key).unwrap();
    assert_eq!(app.registry.get(&route_key), Some(committed));
}

fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

#[tokio::test]
async fn concurrent_post_then_delete_is_serialized_through_the_registry() {
    let store = Arc::new(GatedAtomicAddStore::new());
    let app = test_api_with_store(None, store.clone()).await;
    let post_app = app.clone();
    let posting =
        tokio::spawn(async move { post_route(&post_app, "/api/routes/race", TARGET).await });

    store.entered.acquire().await.unwrap().forget();
    let delete_app = app.clone();
    let mut deleting = Box::pin(request(
        &delete_app,
        "DELETE",
        "/api/routes/race",
        None,
        None,
    ));
    // POST has already entered its store while holding the registry mutation
    // mutex. Polling DELETE starts its registry-owned task, which must serialize
    // behind that POST mutation before it can reach persistence.
    assert!(poll_once(deleting.as_mut()).is_pending());
    store.release.add_permits(1);

    assert_eq!(posting.await.unwrap().status(), StatusCode::CREATED);
    assert_eq!(deleting.await.status(), StatusCode::NO_CONTENT);
    assert!(app
        .registry
        .get(&RouteKey::parse("/race").unwrap())
        .is_none());
}

struct FailingMutationStore {
    routes: BTreeMap<RouteKey, RouteData>,
}

struct IndeterminateApiStore {
    routes: BTreeMap<RouteKey, RouteData>,
}

#[async_trait]
impl Store for IndeterminateApiStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: serde_json::Map<String, Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::Indeterminate { operation: "add" })
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        unreachable!("API fail-stop test only injects add uncertainty")
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!("API fail-stop test only injects add uncertainty")
    }

    async fn update_activity(
        &self,
        _key: &RouteKey,
        _at: chrono::DateTime<Utc>,
    ) -> Result<(), StoreError> {
        unreachable!("API fail-stop test only injects add uncertainty")
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        unreachable!("sealed API must reject before persistence")
    }
}

#[tokio::test]
async fn indeterminate_mutation_makes_management_routes_fixed_empty_503s() {
    let existing_key = RouteKey::parse("/existing").unwrap();
    let store: Arc<dyn Store> = Arc::new(IndeterminateApiStore {
        routes: BTreeMap::from([(
            existing_key,
            RouteData {
                target: TARGET.to_owned(),
                last_activity: Utc.timestamp_opt(1, 0).unwrap(),
                extra: Default::default(),
            },
        )]),
    });
    let app = test_api_with_store(None, store).await;

    let triggering = post_route(&app, "/api/routes/uncertain", TARGET).await;
    assert_eq!(triggering.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(read_bytes(triggering).await.is_empty());

    for (method, path) in [
        ("GET", "/api/routes"),
        ("GET", "/api/routes/existing"),
        ("DELETE", "/api/routes/existing"),
    ] {
        let response = request(&app, method, path, None, None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(read_bytes(response).await.is_empty());
    }
}

struct AtomicAddFailureStore {
    routes: RwLock<BTreeMap<RouteKey, RouteData>>,
}

#[async_trait]
impl Store for AtomicAddFailureStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: serde_json::Map<String, Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        Err(StoreError::message("injected atomic add failure"))
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

    async fn update_activity(
        &self,
        _key: &RouteKey,
        _at: chrono::DateTime<Utc>,
    ) -> Result<(), StoreError> {
        Err(StoreError::message("injected activity failure"))
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Ok(self.routes.write().await.remove(key))
    }
}

#[async_trait]
impl Store for FailingMutationStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: serde_json::Map<String, Value>,
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

    async fn update_activity(
        &self,
        _key: &RouteKey,
        _at: chrono::DateTime<Utc>,
    ) -> Result<(), StoreError> {
        Err(StoreError::message("injected activity failure"))
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Err(StoreError::message("injected delete failure"))
    }
}

#[tokio::test]
async fn metrics_match_completed_responses_and_successful_operation_promises() {
    let app = test_api(None).await;

    assert_eq!(
        request(&app, "GET", "/api/routes", None, None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/api/routes/missing", None, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post_route(&app, "/api/routes/metrics", TARGET)
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        request(&app, "DELETE", "/api/routes/metrics", None, None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&app, "DELETE", "/api/routes/metrics", None, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    let snapshot = app.metrics.snapshot();
    assert_eq!(snapshot.requests_api.get(&200), Some(&1));
    assert_eq!(snapshot.requests_api.get(&201), Some(&1));
    assert_eq!(snapshot.requests_api.get(&204), Some(&1));
    assert_eq!(snapshot.requests_api.get(&404), Some(&2));
    assert_eq!(snapshot.api_route_get, 1);
    assert_eq!(snapshot.api_route_add, 1);
    assert_eq!(snapshot.api_route_delete, 2);
}

#[tokio::test]
async fn failed_store_operations_count_500_responses_but_not_route_operations() {
    let existing_key = RouteKey::parse("/existing").unwrap();
    let store: Arc<dyn Store> = Arc::new(FailingMutationStore {
        routes: BTreeMap::from([(
            existing_key,
            RouteData {
                target: TARGET.to_owned(),
                last_activity: Utc.timestamp_opt(1, 0).unwrap(),
                extra: serde_json::Map::new(),
            },
        )]),
    });
    let app = test_api_with_store(None, store).await;

    assert_eq!(
        post_route(&app, "/api/routes/new", TARGET).await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        request(&app, "DELETE", "/api/routes/existing", None, None)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    let snapshot = app.metrics.snapshot();
    assert_eq!(snapshot.requests_api.get(&500), Some(&2));
    assert_eq!(snapshot.api_route_add, 0);
    assert_eq!(snapshot.api_route_delete, 0);
    assert!(app
        .registry
        .get(&RouteKey::parse("/existing").unwrap())
        .is_some());
    assert!(app
        .registry
        .get(&RouteKey::parse("/new").unwrap())
        .is_none());
}

#[tokio::test]
async fn failed_atomic_post_persistence_never_mutates_publishes_or_counts() {
    let store = Arc::new(AtomicAddFailureStore {
        routes: RwLock::new(BTreeMap::new()),
    });
    let app = test_api_with_store(None, store.clone()).await;

    assert_eq!(
        post_route(&app, "/api/routes/activity-failure", TARGET)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    assert!(app
        .registry
        .get(&RouteKey::parse("/activity-failure").unwrap())
        .is_none());
    assert!(!store
        .snapshot()
        .await
        .unwrap()
        .contains_key(&RouteKey::parse("/activity-failure").unwrap()));
    let snapshot = app.metrics.snapshot();
    assert_eq!(snapshot.requests_api.get(&500), Some(&1));
    assert_eq!(snapshot.api_route_add, 0);
}

#[tokio::test]
async fn requests_api_counts_auth_parse_method_and_fallback_responses() {
    let app = test_api(Some("secret")).await;

    assert_eq!(
        request(&app, "GET", "/api/routes", None, None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request_raw(&app, "POST", "/api/routes/bad", Some(b"{".to_vec()), None,)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "PUT", "/api/routes", None, None)
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        request(&app, "GET", "/api/unknown", None, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    let snapshot = app.metrics.snapshot();
    assert_eq!(snapshot.requests_api.get(&400), Some(&1));
    assert_eq!(snapshot.requests_api.get(&403), Some(&1));
    assert_eq!(snapshot.requests_api.get(&404), Some(&1));
    assert_eq!(snapshot.requests_api.get(&405), Some(&1));
}

#[tokio::test]
async fn both_inactivity_query_spellings_filter_strictly_older_routes() {
    let app = test_api(None).await;
    for (path, seconds) in [("/old", 10), ("/boundary", 20), ("/new", 30)] {
        app.registry
            .put(
                RouteKey::parse(path).unwrap(),
                RouteData {
                    target: TARGET.to_owned(),
                    last_activity: Utc.timestamp_opt(seconds, 0).unwrap(),
                    extra: serde_json::Map::new(),
                },
            )
            .await
            .unwrap();
    }

    for spelling in ["inactiveSince", "inactive_since"] {
        let value = get_json(
            &app,
            &format!("/api/routes?{spelling}=1970-01-01T00%3A00%3A20Z"),
        )
        .await;
        assert_eq!(
            value.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["/old"]
        );
    }

    let fallback = get_json(
        &app,
        "/api/routes?inactiveSince=&inactiveSince=invalid&inactive_since=1970-01-01T00%3A00%3A20Z",
    )
    .await;
    assert_eq!(
        fallback.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["/old"]
    );
}

#[tokio::test]
async fn invalid_inactivity_timestamp_matches_chp_400_body() {
    let app = test_api(None).await;
    let response = request(
        &app,
        "GET",
        "/api/routes?inactiveSince=endoftheuniverse",
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_text(response).await,
        "Invalid datestamp 'endoftheuniverse' must be ISO8601."
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #[test]
    fn arbitrary_post_bytes_never_panic_and_only_target_objects_can_publish(
        body in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        runtime().block_on(async move {
            let app = test_api(None).await;
            let response = request_raw(
                &app,
                "POST",
                "/api/routes/property",
                Some(body.clone()),
                None,
            ).await;
            prop_assert!(response.status().is_client_error() || response.status().is_success());

            let published = app.registry.all().contains_key(&RouteKey::parse("/property").unwrap());
            if published {
                let parsed: Value = serde_json::from_slice(&body).unwrap();
                prop_assert!(parsed.as_object().and_then(|value| value.get("target")).is_some_and(Value::is_string));
            }
            Ok(())
        })?;
    }

    #[test]
    fn arbitrary_percent_sequences_always_return_a_response(
        suffix in prop::string::string_regex("[A-Za-z0-9%]{0,48}").unwrap(),
    ) {
        runtime().block_on(async move {
            let app = test_api(None).await;
            let path = format!("/api/routes/{suffix}");
            let response = request(&app, "GET", &path, None, None).await;
            match response.status() {
                StatusCode::OK => prop_assert!(suffix.is_empty()),
                StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND => {},
                status => prop_assert!(false, "unexpected status {status} for {suffix:?}"),
            }
            prop_assert!(app.registry.all().is_empty());
            Ok(())
        })?;
    }

    #[test]
    fn arbitrary_authorization_headers_always_return_a_response(
        auth in prop::string::string_regex("[ -~]{0,128}").unwrap(),
    ) {
        runtime().block_on(async move {
            let app = test_api(Some("expected-token")).await;
            let response = request(&app, "GET", "/api/routes", None, Some(&auth)).await;
            prop_assert!(matches!(response.status(), StatusCode::OK | StatusCode::FORBIDDEN));
            Ok(())
        })?;
    }

    #[test]
    fn arbitrary_query_timestamps_always_return_a_response(
        timestamp in prop::collection::vec(any::<u8>(), 0..96),
    ) {
        runtime().block_on(async move {
            let app = test_api(None).await;
            let encoded: String = url::form_urlencoded::byte_serialize(&timestamp).collect();
            let path = format!("/api/routes?inactiveSince={encoded}");
            let response = request(&app, "GET", &path, None, None).await;
            prop_assert!(matches!(response.status(), StatusCode::OK | StatusCode::BAD_REQUEST));
            Ok(())
        })?;
    }
}

#[test]
fn generated_string_target_objects_are_the_only_successful_post_class() {
    let mut runner = proptest::test_runner::TestRunner::default();
    let strategy = prop_oneof![
        Just(json!({})),
        any::<i64>().prop_map(|target| json!({ "target": target })),
        "[a-z]{1,16}".prop_map(|target| json!({ "other": target })),
        prop::collection::vec(any::<char>(), 0..32)
            .prop_map(|characters| json!({ "target": characters.into_iter().collect::<String>() })),
    ];

    runner
        .run(&strategy, |body| {
            runtime().block_on(async {
                let app = test_api(None).await;
                let expected_success = body
                    .as_object()
                    .and_then(|object| object.get("target"))
                    .and_then(Value::as_str)
                    .is_some();
                let response =
                    request(&app, "POST", "/api/routes/generated", Some(body), None).await;
                prop_assert_eq!(response.status().is_success(), expected_success);
                prop_assert_eq!(app.registry.all().len(), usize::from(expected_success));
                Ok(())
            })
        })
        .unwrap();
}
