mod support;

use chrono::{TimeZone, Utc};
use proptest::prelude::*;
use serde_json::{json, Value};
use url::Url;

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

    let response = request_raw(
        &app,
        "POST",
        "/api/routes/rejected",
        Some(b"{ definitely not json".to_vec()),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(read_text(response)
        .await
        .starts_with("Body not valid JSON: "));
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
async fn both_inactivity_query_spellings_filter_strictly_older_routes() {
    let app = test_api(None).await;
    for (path, seconds) in [("/old", 10), ("/boundary", 20), ("/new", 30)] {
        app.registry
            .put(
                RouteKey::parse(path).unwrap(),
                RouteData {
                    target: Url::parse(TARGET).unwrap(),
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
            prop_assert!(response.status().as_u16() >= 200);
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
fn generated_valid_target_objects_are_the_only_successful_post_class() {
    let mut runner = proptest::test_runner::TestRunner::default();
    let strategy = prop_oneof![
        Just(json!({})),
        any::<i64>().prop_map(|target| json!({ "target": target })),
        "[a-z]{1,16}".prop_map(|target| json!({ "other": target })),
        (1024u16..65535).prop_map(|port| json!({ "target": format!("http://127.0.0.1:{port}/") })),
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
