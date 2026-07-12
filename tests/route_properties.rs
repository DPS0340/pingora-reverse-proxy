use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use pingora_reverse_proxy::route::{RouteData, RouteKey};
use pingora_reverse_proxy::route_table::{RouteMatch, RouteSnapshot};
use proptest::collection::btree_map;
use proptest::prelude::*;
use serde_json::{json, Map, Value};
use url::Url;

fn route_data(id: usize) -> RouteData {
    RouteData {
        target: Url::parse(&format!("http://upstream-{id}.example/")).unwrap(),
        last_activity: Utc.timestamp_opt(id as i64, 0).unwrap(),
        extra: Map::from_iter([("route_id".to_owned(), json!(id))]),
    }
}

fn snapshot_with<const N: usize>(keys: [&str; N]) -> RouteSnapshot {
    let routes = keys
        .into_iter()
        .enumerate()
        .map(|(id, key)| (RouteKey::parse(key).unwrap(), route_data(id)))
        .collect();
    RouteSnapshot::from_routes(routes)
}

fn test_segments(path: &str) -> Vec<&str> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    }
}

fn reference_resolve(routes: &BTreeMap<RouteKey, RouteData>, request: &str) -> Option<RouteKey> {
    let request_segments = test_segments(request);

    routes
        .keys()
        .filter_map(|key| {
            let route_segments = test_segments(key.as_str());
            request_segments
                .starts_with(&route_segments)
                .then_some((route_segments.len(), key))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, key)| key.clone())
}

fn segment_strategy() -> impl Strategy<Value = String> {
    "[a-z0-9_-]{1,8}".prop_map(String::from)
}

fn segmented_path_strategy(max_segments: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(segment_strategy(), 0..=max_segments).prop_map(|segments| {
        if segments.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}", segments.join("/"))
        }
    })
}

fn route_key_strategy() -> impl Strategy<Value = String> {
    (segmented_path_strategy(5), 0usize..=3, any::<bool>()).prop_map(
        |(path, trailing_slashes, omit_leading_slash)| {
            let path = if omit_leading_slash {
                path.trim_start_matches('/').to_owned()
            } else {
                path
            };
            format!("{path}{}", "/".repeat(trailing_slashes))
        },
    )
}

fn route_map_strategy() -> impl Strategy<Value = BTreeMap<RouteKey, RouteData>> {
    btree_map(segmented_path_strategy(4), 0usize..64, 0..32).prop_map(|routes| {
        routes
            .into_iter()
            .map(|(key, id)| (RouteKey::parse(&key).unwrap(), route_data(id)))
            .collect()
    })
}

fn request_path_strategy() -> impl Strategy<Value = String> {
    (segmented_path_strategy(6), 0usize..=3, any::<bool>()).prop_map(
        |(path, trailing_slashes, omit_leading_slash)| {
            let path = if omit_leading_slash {
                path.trim_start_matches('/').to_owned()
            } else {
                path
            };
            format!("{path}{}", "/".repeat(trailing_slashes))
        },
    )
}

#[test]
fn route_key_normalizes_leading_and_trailing_slashes() {
    let cases = [
        ("", "/"),
        ("/", "/"),
        ("route", "/route"),
        ("/route/", "/route"),
        ("/route///", "/route"),
    ];

    for (raw, expected) in cases {
        assert_eq!(RouteKey::parse(raw).unwrap().as_str(), expected);
    }
}

#[test]
fn route_data_round_trips_unknown_fields() {
    let source = json!({
        "target": "http://upstream.example/base",
        "last_activity": "2026-07-12T10:30:00Z",
        "hub_user": "alice",
        "server_name": "lab"
    });

    let route: RouteData = serde_json::from_value(source.clone()).unwrap();

    assert_eq!(route.extra["hub_user"], Value::String("alice".to_owned()));
    assert_eq!(serde_json::to_value(route).unwrap(), source);
}

#[test]
fn root_route_is_the_fallback() {
    let snapshot = snapshot_with(["/", "/users/alice"]);

    assert_eq!(snapshot.resolve("/unmatched").unwrap().key.as_str(), "/");
}

#[test]
fn deepest_route_with_data_wins() {
    let snapshot = snapshot_with(["/", "/a", "/a/b/c"]);

    assert_eq!(snapshot.resolve("/a/b").unwrap().key.as_str(), "/a");
    assert_eq!(
        snapshot.resolve("/a/b/c/more").unwrap().key.as_str(),
        "/a/b/c"
    );
}

#[test]
fn unmatched_path_without_root_returns_none() {
    let snapshot = snapshot_with(["/known"]);

    assert!(snapshot.resolve("/missing").is_none());
}

#[test]
fn segment_boundary_prevents_partial_match() {
    let snapshot = snapshot_with(["/b/c", "/b/c/d"]);
    assert_eq!(snapshot.resolve("/b/c/dword").unwrap().key.as_str(), "/b/c");
}

#[test]
fn interior_empty_segments_are_significant() {
    let snapshot = snapshot_with(["/a/b", "/a//b"]);

    assert_eq!(
        snapshot.resolve("/a//b/more").unwrap().key.as_str(),
        "/a//b"
    );
    assert_eq!(snapshot.resolve("/a/b/more").unwrap().key.as_str(), "/a/b");
}

#[test]
fn route_match_carries_shared_route_data() {
    let snapshot = snapshot_with(["/shared"]);

    let matched: RouteMatch = snapshot.resolve("/shared/more").unwrap();
    assert_eq!(matched.data.extra["route_id"], json!(0));
}

proptest! {
    #[test]
    fn normalization_is_idempotent(raw in route_key_strategy()) {
        let once = RouteKey::parse(&raw).unwrap();
        let twice = RouteKey::parse(once.as_str()).unwrap();
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn optimized_matcher_equals_reference(
        routes in route_map_strategy(),
        request in request_path_strategy(),
    ) {
        let snapshot = RouteSnapshot::from_routes(routes.clone());
        prop_assert_eq!(
            snapshot.resolve(&request).map(|matched| matched.key),
            reference_resolve(&routes, &request),
        );
    }
}
