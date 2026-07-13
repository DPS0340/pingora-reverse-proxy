#[path = "support/oracle.rs"]
mod oracle;
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use axum::body::Body;
use futures_util::{SinkExt, StreamExt};
use http::Request;
use pingora_reverse_proxy::api_server::metrics_router;
use serde_json::json;
use tower::ServiceExt;

use oracle::{
    assert_equal_with_diagnostics_for_test, compare_metric_deltas_for_test,
    compare_metric_expositions, observation_for_context_test, observation_for_test,
    oracle_run_owner_label_is_valid_for_test, route_body_for_test, CapturedProcess,
    EndpointMapping, LaunchLock, ObservationContext, ObservationSide, OraclePair, PortLease,
};
use support::{read_text, request, test_api, StatusCode};

/// The complete normalization allowlist for differential observations:
///
/// - HTTP `Date` header values;
/// - server-generated connection/framing headers (`Connection`, `Keep-Alive`,
///   `Transfer-Encoding`, and `Content-Length`);
/// - listener/client addresses and ports allocated ephemerally by the harness.
///
/// Route keys, request paths and queries, status codes, semantic headers,
/// JSON metadata, bodies, metric family/label names, and TLS outcomes are never
/// normalized.
const NORMALIZATION_ALLOWLIST: &[&str] = &[
    "HTTP Date header values",
    "server-generated connection/framing headers",
    "ephemeral addresses",
];

const CHP_METRIC_FAMILIES: &[(&str, &str)] = &[
    ("api_route_get", "counter"),
    ("api_route_add", "counter"),
    ("api_route_delete", "counter"),
    ("find_target_for_req", "summary"),
    ("last_activity_updating", "summary"),
    ("requests_ws", "counter"),
    ("requests_web", "counter"),
    ("requests_proxy", "counter"),
    ("requests_api", "counter"),
];

#[test]
fn oracle_owner_label_is_required_and_strictly_shaped() {
    assert!(oracle_run_owner_label_is_valid_for_test(Some(
        "io.openrusty.chp-differential.run=chp-diff-123-456"
    )));
    for invalid in [
        None,
        Some(""),
        Some("io.openrusty.chp-differential.run="),
        Some("io.openrusty.chp-differential.run=other-run"),
        Some("io.openrusty.chp-differential.run=chp-diff-bad=value"),
        Some("io.openrusty.chp-differential.run=chp-diff-bad\nlabel"),
        Some("other.label=chp-diff-123-456"),
    ] {
        assert!(!oracle_run_owner_label_is_valid_for_test(invalid));
    }
}

fn comparator_mappings() -> Vec<EndpointMapping> {
    vec![
        EndpointMapping::new("public", "127.0.0.1:11001", "127.0.0.1:21001"),
        EndpointMapping::new("api", "127.0.0.1:11002", "127.0.0.1:21002"),
        EndpointMapping::new("metrics", "127.0.0.1:11003", "127.0.0.1:21003"),
        EndpointMapping::new("echo", "127.0.0.1:11004", "127.0.0.1:21004"),
    ]
}

#[test]
fn comparator_keeps_endpoint_roles_distinct_and_rejects_swaps() {
    let mappings = comparator_mappings();
    let chp = observation_for_context_test(
        ObservationContext::RouteMutation,
        ObservationSide::Chp,
        200,
        &[],
        br#"{"target":"http://127.0.0.1:11001/base"}"#,
        &mappings,
    );
    let matching_rust = observation_for_context_test(
        ObservationContext::RouteMutation,
        ObservationSide::Rust,
        200,
        &[],
        br#"{"target":"http://127.0.0.1:21001/base"}"#,
        &mappings,
    );
    let swapped_rust = observation_for_context_test(
        ObservationContext::RouteMutation,
        ObservationSide::Rust,
        200,
        &[],
        br#"{"target":"http://127.0.0.1:21002/base"}"#,
        &mappings,
    );
    assert_eq!(chp, matching_rust);
    assert_ne!(chp, swapped_rust);
}

#[test]
fn comparator_preserves_duplicate_and_non_utf8_semantic_headers() {
    let mappings = comparator_mappings();
    let duplicated = observation_for_test(
        ObservationSide::Chp,
        200,
        &[("x-semantic", b"one"), ("x-semantic", b"two")],
        b"ok",
        &mappings,
    );
    let collapsed = observation_for_test(
        ObservationSide::Rust,
        200,
        &[("x-semantic", b"two")],
        b"ok",
        &mappings,
    );
    assert_ne!(duplicated, collapsed);

    let ordered = observation_for_test(
        ObservationSide::Chp,
        200,
        &[("x-semantic", b"one"), ("x-semantic", b"two")],
        b"ok",
        &mappings,
    );
    let reversed = observation_for_test(
        ObservationSide::Rust,
        200,
        &[("x-semantic", b"two"), ("x-semantic", b"one")],
        b"ok",
        &mappings,
    );
    assert_ne!(ordered, reversed, "duplicate header order is semantic");

    let non_utf8_a = observation_for_test(
        ObservationSide::Chp,
        200,
        &[("x-binary", &[0xff, 0x00, b'a'])],
        b"ok",
        &mappings,
    );
    let non_utf8_b = observation_for_test(
        ObservationSide::Rust,
        200,
        &[("x-binary", &[0xff, 0x00, b'b'])],
        b"ok",
        &mappings,
    );
    assert_ne!(non_utf8_a, non_utf8_b);
}

#[test]
fn comparator_never_rewrites_user_body_path_query_or_target_suffixes() {
    let mappings = comparator_mappings();
    for (left, right) in [
        (
            br#"{"body":"127.0.0.1:11001"}"#.as_slice(),
            br#"{"body":"127.0.0.1:21001"}"#.as_slice(),
        ),
        (
            br#"{"url":"/path/127.0.0.1:11001?q=11002"}"#.as_slice(),
            br#"{"url":"/path/127.0.0.1:21001?q=21002"}"#.as_slice(),
        ),
        (
            br#"{"target":"http://127.0.0.1:11001/one"}"#.as_slice(),
            br#"{"target":"http://127.0.0.1:21001/two"}"#.as_slice(),
        ),
    ] {
        assert_ne!(
            observation_for_test(ObservationSide::Chp, 200, &[], left, &mappings),
            observation_for_test(ObservationSide::Rust, 200, &[], right, &mappings)
        );
    }
}

#[test]
fn comparator_rejects_nested_user_fields_even_when_they_look_structural() {
    let mappings = comparator_mappings();
    let chp = r#"{
        "metadata": {
            "target": "http://127.0.0.1:11004/private",
            "host": "127.0.0.1:11001",
            "last_activity": "2025-01-01T00:00:00Z"
        }
    }"#;
    let rust = br#"{
        "metadata": {
            "target": "http://127.0.0.1:21004/private",
            "host": "127.0.0.1:21001",
            "last_activity": "2026-01-01T00:00:00Z"
        }
    }"#;
    assert_ne!(
        observation_for_test(ObservationSide::Chp, 200, &[], chp.as_bytes(), &mappings),
        observation_for_test(ObservationSide::Rust, 200, &[], rust, &mappings),
        "unknown metadata must never be normalized recursively"
    );
}

#[test]
fn route_mutation_preparation_rewrites_only_the_root_target_sentinel() {
    let prepared = route_body_for_test(
        json!({
            "target": "http://oracle-echo.invalid/base",
            "metadata": {
                "target": "http://oracle-echo.invalid/user-data",
                "host": "oracle-echo.invalid",
                "last_activity": "2024-02-03T04:05:06Z"
            }
        }),
        ObservationSide::Chp,
        43210,
    );
    assert_eq!(prepared["target"], "http://host.docker.internal:43210/base");
    assert_eq!(
        prepared["metadata"]["target"],
        "http://oracle-echo.invalid/user-data"
    );
    assert_eq!(prepared["metadata"]["host"], "oracle-echo.invalid");
    assert_eq!(
        prepared["metadata"]["last_activity"],
        "2024-02-03T04:05:06Z"
    );
    assert_eq!(prepared["last_activity"], "2100-01-01T00:00:00.000Z");

    let user_target = route_body_for_test(
        json!({ "target": "http://example.test/oracle-echo.invalid/user-data" }),
        ObservationSide::Chp,
        43210,
    );
    assert_eq!(
        user_target["target"],
        "http://example.test/oracle-echo.invalid/user-data"
    );
}

#[test]
fn route_table_context_normalizes_only_direct_route_targets() {
    let mappings = comparator_mappings();
    let chp = r#"{
        "/route": {
            "target": "http://127.0.0.1:11004/base",
            "last_activity": "2000-01-01T00:00:00.000Z",
            "metadata": {"target": "http://127.0.0.1:11004/user"}
        }
    }"#;
    let matching_rust = r#"{
        "/route": {
            "target": "http://127.0.0.1:21004/base",
            "last_activity": "2000-01-01T00:00:00.000Z",
            "metadata": {"target": "http://127.0.0.1:11004/user"}
        }
    }"#;
    let nested_target_changed =
        matching_rust.replace("127.0.0.1:11004/user", "127.0.0.1:21004/user");
    let direct_target_path_changed = matching_rust.replace(
        "http://127.0.0.1:21004/base",
        "http://127.0.0.1:21004/different",
    );
    let last_activity_changed =
        matching_rust.replace("2000-01-01T00:00:00.000Z", "2001-01-01T00:00:00.000Z");
    let metadata_changed =
        matching_rust.replace("\"metadata\": {", "\"metadata\": {\"arbitrary\": true,");
    let observe = |side, body: &[u8]| {
        observation_for_context_test(
            ObservationContext::RouteTable,
            side,
            200,
            &[],
            body,
            &mappings,
        )
    };
    assert_eq!(
        observe(ObservationSide::Chp, chp.as_bytes()),
        observe(ObservationSide::Rust, matching_rust.as_bytes())
    );
    assert_ne!(
        observe(ObservationSide::Chp, chp.as_bytes()),
        observe(ObservationSide::Rust, nested_target_changed.as_bytes())
    );
    assert_ne!(
        observe(ObservationSide::Chp, chp.as_bytes()),
        observe(ObservationSide::Rust, direct_target_path_changed.as_bytes()),
        "route target paths remain exact after endpoint normalization"
    );
    assert_ne!(
        observe(ObservationSide::Chp, chp.as_bytes()),
        observe(ObservationSide::Rust, last_activity_changed.as_bytes())
    );
    assert_ne!(
        observe(ObservationSide::Chp, chp.as_bytes()),
        observe(ObservationSide::Rust, metadata_changed.as_bytes()),
        "arbitrary route metadata remains semantic"
    );
}

#[test]
fn crud_scenarios_compare_populated_route_tables_at_each_mutation() {
    let source = include_str!("differential.rs");
    let tcp = source
        .split_once(
            "#[tokio::test]\nasync fn chp_and_rust_agree_on_api_crud_and_encoded_unicode_route()",
        )
        .expect("TCP CRUD scenario")
        .1
        .split_once("#[tokio::test]\nasync fn chp_and_rust_agree_on_longest_http_route_selection()")
        .expect("next TCP scenario")
        .0;
    assert_eq!(
        tcp.matches("pair.assert_same_route_tables().await;")
            .count(),
        3,
        "TCP CRUD must compare live tables after CREATE, UPDATE, and DELETE"
    );

    let uds = source
        .split_once(
            "#[tokio::test]\nasync fn chp_and_rust_agree_on_unix_public_api_and_metrics_sockets()",
        )
        .expect("UDS CRUD scenario")
        .1;
    assert_eq!(
        uds.matches("pair.assert_same_route_tables().await;")
            .count(),
        2,
        "UDS CRUD must compare live tables after CREATE and DELETE"
    );
}

const EXACT_METRICS: &str = "# HELP api_route_get Count of API route get requests\n# TYPE api_route_get counter\napi_route_get 1\n\n# HELP api_route_add Count of API route add requests\n# TYPE api_route_add counter\napi_route_add 2\n\n# HELP api_route_delete Count of API route delete requests\n# TYPE api_route_delete counter\napi_route_delete 1\n\n# HELP find_target_for_req Summary of find target requests\n# TYPE find_target_for_req summary\nfind_target_for_req{quantile=\"0.01\"} 0.1\nfind_target_for_req{quantile=\"0.05\"} 0.1\nfind_target_for_req{quantile=\"0.5\"} 0.2\nfind_target_for_req{quantile=\"0.9\"} 0.3\nfind_target_for_req{quantile=\"0.95\"} 0.3\nfind_target_for_req{quantile=\"0.99\"} 0.3\nfind_target_for_req{quantile=\"0.999\"} 0.3\nfind_target_for_req_sum 0.6\nfind_target_for_req_count 3\n\n# HELP last_activity_updating Summary of last activity updating requests\n# TYPE last_activity_updating summary\nlast_activity_updating{quantile=\"0.01\"} 0.1\nlast_activity_updating{quantile=\"0.05\"} 0.1\nlast_activity_updating{quantile=\"0.5\"} 0.1\nlast_activity_updating{quantile=\"0.9\"} 0.1\nlast_activity_updating{quantile=\"0.95\"} 0.1\nlast_activity_updating{quantile=\"0.99\"} 0.1\nlast_activity_updating{quantile=\"0.999\"} 0.1\nlast_activity_updating_sum 0.1\nlast_activity_updating_count 1\n\n# HELP requests_ws Count of websocket requests\n# TYPE requests_ws counter\nrequests_ws 1\n\n# HELP requests_web Count of web requests\n# TYPE requests_web counter\nrequests_web 2\n\n# HELP requests_proxy Count of proxy requests\n# TYPE requests_proxy counter\nrequests_proxy{status=\"200\"} 1\nrequests_proxy{status=\"503\"} 2\n\n# HELP requests_api Count of API requests\n# TYPE requests_api counter\nrequests_api{status=\"200\"} 3\nrequests_api{status=\"201\"} 2\n";

#[test]
fn metric_comparator_rejects_unexpected_families_help_types_labels_and_values() {
    assert!(compare_metric_expositions(EXACT_METRICS, EXACT_METRICS).is_ok());
    for changed in [
        EXACT_METRICS.to_owned() + "\n# HELP surprise nope\n# TYPE surprise counter\nsurprise 1\n",
        EXACT_METRICS.replacen("Count of web requests", "Wrong help", 1),
        EXACT_METRICS.replacen("requests_ws counter", "requests_ws gauge", 1),
        EXACT_METRICS.replacen("requests_web 2", "requests_web 3", 1),
        EXACT_METRICS.replacen(
            "requests_proxy{status=\"503\"} 2",
            "requests_proxy{status=\"500\"} 2",
            1,
        ),
        EXACT_METRICS.replacen(
            "requests_api{status=\"201\"} 2",
            "requests_api{status=\"201\"} 2\nrequests_api{status=\"201\"} 2",
            1,
        ),
    ] {
        assert!(
            compare_metric_expositions(EXACT_METRICS, &changed).is_err(),
            "metric comparator accepted a semantic mutation"
        );
    }
}

#[test]
fn metric_comparator_rejects_runtime_families_from_rust_only() {
    let runtime = "\n# HELP process_cpu_seconds_total CPU\n# TYPE process_cpu_seconds_total counter\nprocess_cpu_seconds_total 1\n";
    assert!(
        compare_metric_expositions(&(EXACT_METRICS.to_owned() + runtime), EXACT_METRICS).is_ok()
    );
    assert!(
        compare_metric_expositions(EXACT_METRICS, &(EXACT_METRICS.to_owned() + runtime)).is_err(),
        "the CHP-only runtime allowlist must not apply to Rust"
    );
}

#[test]
fn metric_comparator_rejects_invalid_summary_structure_even_on_both_sides() {
    let mut mutations = vec![
        EXACT_METRICS.replacen(
            "find_target_for_req{quantile=\"0.01\"} 0.1\n",
            "",
            1,
        ),
        EXACT_METRICS.replacen(
            "find_target_for_req{quantile=\"0.01\"} 0.1",
            "find_target_for_req{quantile=\"0.01\"} 0.1\nfind_target_for_req{quantile=\"0.01\"} 0.1",
            1,
        ),
        EXACT_METRICS.replacen(
            "find_target_for_req{quantile=\"0.5\"} 0.2",
            "find_target_for_req{quantile=\"0.5\"} -0.2",
            1,
        ),
        EXACT_METRICS.replacen(
            "find_target_for_req{quantile=\"0.9\"} 0.3",
            "find_target_for_req{quantile=\"0.9\"} 0.05",
            1,
        ),
        EXACT_METRICS.replacen("find_target_for_req_sum 0.6", "find_target_for_req_sum -0.6", 1),
        EXACT_METRICS.replacen("find_target_for_req_count 3", "find_target_for_req_count 3.5", 1),
        EXACT_METRICS.replacen(
            "find_target_for_req{quantile=\"0.5\"} 0.2",
            "find_target_for_req{quantile=\"0.5\",unexpected=\"x\"} 0.2",
            1,
        ),
    ];
    mutations.push(EXACT_METRICS.replacen(
        "find_target_for_req_count 3",
        "find_target_for_req_count 18446744073709551616",
        1,
    ));
    for value in ["NaN", "+Inf", "-Inf"] {
        mutations.push(EXACT_METRICS.replacen(
            "find_target_for_req{quantile=\"0.5\"} 0.2",
            &format!("find_target_for_req{{quantile=\"0.5\"}} {value}"),
            1,
        ));
    }
    for invalid in mutations {
        assert!(
            compare_metric_expositions(&invalid, &invalid).is_err(),
            "accepted an invalid summary on both sides"
        );
    }
    for value in ["NaN", "+Inf", "-Inf"] {
        let invalid = EXACT_METRICS.replacen(
            "find_target_for_req_sum 0.6",
            &format!("find_target_for_req_sum {value}"),
            1,
        );
        assert!(compare_metric_expositions(&invalid, &invalid).is_err());
    }
}

#[test]
fn metric_delta_comparator_rejects_decreasing_summary_counts() {
    let decreased = EXACT_METRICS.replacen(
        "find_target_for_req_count 3",
        "find_target_for_req_count 2",
        1,
    );
    assert!(
        compare_metric_deltas_for_test(EXACT_METRICS, &decreased, EXACT_METRICS, &decreased)
            .is_err(),
        "matching decreases on both sides must not be accepted as a zero delta"
    );
}

fn fake_chp_source(name: &str, version: &str) -> tempfile::TempDir {
    let source = tempfile::tempdir().expect("fake CHP source");
    std::fs::create_dir_all(source.path().join("lib")).expect("fake CHP lib");
    std::fs::create_dir_all(source.path().join("bin")).expect("fake CHP bin");
    std::fs::write(
        source.path().join("package.json"),
        serde_json::json!({"name": name, "version": version}).to_string(),
    )
    .expect("fake CHP package");
    std::fs::write(
        source.path().join("lib/configproxy.js"),
        "export default {};\n",
    )
    .expect("fake CHP implementation");
    std::fs::write(
        source.path().join("bin/configurable-http-proxy"),
        "export default {};\n",
    )
    .expect("fake CHP CLI");
    source
}

fn run_host_launcher(source: &std::path::Path) -> std::process::Output {
    std::process::Command::new("node")
        .arg(fixture_path("scripts/chp-oracle.mjs"))
        .env("CHP_SOURCE_DIR", source)
        .output()
        .expect("run host launcher negative")
}

#[test]
fn oracle_launcher_rejects_wrong_package_name_and_version() {
    for (name, version) in [
        ("not-configurable-http-proxy", "5.3.0"),
        ("configurable-http-proxy", "5.3.1"),
    ] {
        let source = fake_chp_source(name, version);
        let output = run_host_launcher(source.path());
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("expected configurable-http-proxy 5.3.0"),
            "unexpected launcher diagnostic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn oracle_launcher_rejects_escaped_source_members() {
    use std::os::unix::fs::symlink;

    let source = fake_chp_source("configurable-http-proxy", "5.3.0");
    let outside = tempfile::NamedTempFile::new().expect("outside CHP member");
    std::fs::remove_file(source.path().join("lib/configproxy.js")).expect("remove fake member");
    symlink(outside.path(), source.path().join("lib/configproxy.js")).expect("escape CHP member");
    let output = run_host_launcher(source.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("escaped the pinned source directory"));
}

#[test]
fn oracle_launcher_rejects_global_node_modules_source_attempts() {
    let root = tempfile::tempdir().expect("global-source root");
    let source = root
        .path()
        .join("node_modules")
        .join("configurable-http-proxy");
    std::fs::create_dir_all(source.join("lib")).expect("global fake lib");
    std::fs::create_dir_all(source.join("bin")).expect("global fake bin");
    std::fs::write(
        source.join("package.json"),
        r#"{"name":"configurable-http-proxy","version":"5.3.0"}"#,
    )
    .expect("global fake package");
    std::fs::write(source.join("lib/configproxy.js"), "export default {};\n")
        .expect("global fake implementation");
    std::fs::write(
        source.join("bin/configurable-http-proxy"),
        "export default {};\n",
    )
    .expect("global fake CLI");
    let output = run_host_launcher(&source);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("global node_modules source"));
}

#[test]
fn node_major_validator_is_deterministic_without_using_the_host_runtime() {
    let output = std::process::Command::new("node")
        .args([
            "--input-type=module",
            "--eval",
            "import { validateNodeMajor } from './scripts/chp-runtime.mjs'; validateNodeMajor('v21.99.0');",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run pure Node-major validator");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires Node 20, got v21.99.0"));
}

#[test]
fn oracle_port_lease_holds_the_listener_until_child_launch() {
    let lease = PortLease::new();
    let port = lease.port();
    assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
    drop(lease);
    std::net::TcpListener::bind(("127.0.0.1", port))
        .expect("released differential port can be bound again");
}

#[test]
fn oracle_port_lease_uses_non_ephemeral_handoff_ports() {
    let lease = PortLease::new();
    assert!(
        (20_000..30_000).contains(&lease.port()),
        "listener handoff ports must not be reusable as ephemeral client source ports"
    );
}

#[test]
fn oracle_launch_lock_subprocess_helper() {
    let Ok(lock_path) = std::env::var("ORACLE_LOCK_HELPER_PATH") else {
        return;
    };
    let port: u16 = std::env::var("ORACLE_LOCK_HELPER_PORT")
        .expect("helper port")
        .parse()
        .expect("numeric helper port");
    let ready = std::env::var("ORACLE_LOCK_HELPER_READY").expect("helper ready path");
    let _lock = LaunchLock::acquire_at(std::path::Path::new(&lock_path));
    let _listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .expect("serialized helper owns released port");
    std::fs::write(ready, b"ready").expect("publish helper bind readiness");
    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[test]
fn cross_process_launch_barrier_holds_lease_until_serialized_binder_can_run() {
    let directory = tempfile::tempdir().expect("launch barrier directory");
    let lock_path = directory.path().join("launch.lock");
    let ready = directory.path().join("ready");
    // Keep concurrent oracle launches from claiming the released handoff port
    // while this private-lock subprocess is waking up to bind it.
    let _suite_lock = LaunchLock::acquire();
    let lock = LaunchLock::acquire_at(&lock_path);
    let lease = PortLease::new();
    let port = lease.port();
    let mut helper = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "oracle_launch_lock_subprocess_helper",
            "--nocapture",
        ])
        .env("ORACLE_LOCK_HELPER_PATH", &lock_path)
        .env("ORACLE_LOCK_HELPER_PORT", port.to_string())
        .env("ORACLE_LOCK_HELPER_READY", &ready)
        .spawn()
        .expect("spawn serialized port binder");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!ready.exists(), "binder crossed the process launch lock");
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
        "port lease was released before the child handoff"
    );
    drop(lease);
    drop(lock);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "helper never bound port"
        );
        std::thread::yield_now();
    }
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
        "serialized child did not own the handed-off port"
    );
    assert!(helper.wait().expect("serialized helper exit").success());
}

#[test]
fn captured_process_stderr_tail_is_live_bounded_and_redacted() {
    let workspace = env!("CARGO_MANIFEST_DIR");
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("printf '%s\\n' \"$1\" >&2; i=0; while [ $i -lt 8000 ]; do printf 'diagnostic-%08d-xxxxxxxx\\n' \"$i\" >&2; i=$((i+1)); done; sleep 1")
        .arg("stderr-tail")
        .arg(workspace);
    let mut process = CapturedProcess::spawn(command);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let tail = process.stderr_tail();
        if tail.len() > 60_000 {
            assert!(tail.len() <= 64 * 1024);
            assert!(!tail.contains(workspace));
            assert!(tail.contains("diagnostic-"));
            assert!(
                !process.exited(),
                "tail was available only after process exit"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stderr tail was not drained live"
        );
        std::thread::yield_now();
    }
}

#[test]
fn captured_process_redacts_a_workspace_path_split_across_pipe_reads() {
    let workspace = env!("CARGO_MANIFEST_DIR");
    let split = workspace.len() / 2;
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("printf '%s' \"$1\" >&2; sleep 0.05; printf '%s\\n' \"$2\" >&2; sleep 0.2")
        .arg("stderr-split")
        .arg(&workspace[..split])
        .arg(&workspace[split..]);
    let process = CapturedProcess::spawn(command);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let tail = process.stderr_tail();
    assert!(!tail.contains(workspace), "split workspace leaked: {tail}");
    assert!(
        tail.contains("<workspace>"),
        "assembled tail was not redacted: {tail}"
    );
    assert!(tail.len() <= 64 * 1024);

    let secret = "task11-split-super-secret";
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("printf '%s' \"$1\" >&2; sleep 0.05; printf '%s\\n' \"$2\" >&2; sleep 0.2")
        .arg("stderr-secret-split")
        .arg(&secret[..10])
        .arg(&secret[10..])
        .env("CONFIGPROXY_AUTH_TOKEN", secret);
    let process = CapturedProcess::spawn(command);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let tail = process.stderr_tail();
    assert!(!tail.contains(secret), "split secret leaked: {tail}");
    assert!(
        tail.contains("<redacted>"),
        "assembled secret was not redacted: {tail}"
    );
}

#[test]
fn ordinary_assertion_diagnostics_include_both_bounded_redacted_tails() {
    for scenario in [
        "HTTP response",
        "route table",
        "WebSocket",
        "TLS/mTLS",
        "UDS",
        "metrics",
        "readiness",
    ] {
        let panic = std::panic::catch_unwind(|| {
            assert_equal_with_diagnostics_for_test(
                scenario,
                &"chp-value",
                &"rust-value",
                "chp-tail",
                "rust-tail",
            );
        })
        .expect_err("mismatched ordinary assertion should panic");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("diagnostic panic text");
        assert!(message.contains(scenario));
        assert!(message.contains("CHP stderr tail=chp-tail"));
        assert!(message.contains("Rust stderr tail=rust-tail"));
    }
}

#[test]
fn oracle_image_runs_actual_node_20_and_exact_chp_source() {
    let image = std::env::var("CHP_ORACLE_IMAGE").expect("pinned oracle image from gate script");
    let output = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            &image,
            "node",
            "/usr/local/bin/chp-oracle.mjs",
            "--runtime-probe",
        ])
        .output()
        .expect("probe pinned oracle image");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probe: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("runtime probe JSON");
    assert!(probe["node"]
        .as_str()
        .is_some_and(|node| node.starts_with("v20.")));
    assert_eq!(probe["package"], "configurable-http-proxy@5.3.0");
    assert_eq!(probe["source"], "/opt/chp-5.3.0");
}

struct CustomErrorFixture {
    address: std::net::SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl CustomErrorFixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind custom-error fixture");
        let address = listener.local_addr().expect("custom-error address");
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let router =
                axum::Router::new().fallback(axum::routing::any(|uri: http::Uri| async move {
                    (
                        [(http::header::CONTENT_TYPE, "text/plain")],
                        format!("custom:{uri}"),
                    )
                }));
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve custom-error fixture");
        });
        Self {
            address,
            stop: Some(stop),
            task,
        }
    }

    fn target(&self) -> String {
        format!("http://{}/errors/", self.address)
    }
}

impl Drop for CustomErrorFixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

struct WebSocketFixture {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl WebSocketFixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind websocket fixture");
        let address = listener.local_addr().expect("websocket fixture address");
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream)
                        .await
                        .expect("accept fixture websocket");
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            "connected".into(),
                        ))
                        .await
                        .expect("send fixture greeting");
                    while let Some(message) = socket.next().await {
                        let message = message.expect("fixture websocket message");
                        match message {
                            tokio_tungstenite::tungstenite::Message::Binary(bytes) => socket
                                .send(tokio_tungstenite::tungstenite::Message::Binary(bytes))
                                .await
                                .expect("echo fixture binary"),
                            tokio_tungstenite::tungstenite::Message::Close(frame) => {
                                let _ = frame;
                                break;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        Self { address, task }
    }

    fn target(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for WebSocketFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn metrics_expose_exact_chp_families_and_status_labels() {
    let app = test_api(None).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/routes/metric",
            Some(json!({ "target": "http://127.0.0.1:9" })),
            None,
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        request(&app, "GET", "/api/routes", None, None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "DELETE", "/api/routes/metric", None, None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );

    let response = metrics_router(app.metrics)
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("metrics request"),
        )
        .await
        .expect("metrics router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let body = read_text(response).await;

    for (family, kind) in CHP_METRIC_FAMILIES {
        assert!(
            body.contains(&format!("# TYPE {family} {kind}\n")),
            "missing exact CHP metric family {family}:\n{body}"
        );
    }
    assert!(body.contains("requests_api{status=\"201\"} 1\n"));
    assert!(body.contains("requests_api{status=\"200\"} 1\n"));
    assert!(body.contains("requests_api{status=\"204\"} 1\n"));
    assert!(body.contains("last_activity_updating_count 1\n"));
    assert!(!body.contains("requests_api{code="));
    assert!(!body.contains("requests_proxy{code="));
}

#[tokio::test]
async fn chp_and_rust_agree_on_api_crud_and_encoded_unicode_route() {
    assert_eq!(NORMALIZATION_ALLOWLIST.len(), 3);
    let pair = OraclePair::start().await;
    pair.post_route(
        "/user/%E7%A7%80%E6%A8%B9",
        json!({
            "target": pair.echo_target(),
            "jupyterhub": true,
            "user": "秀樹"
        }),
    )
    .await;
    pair.assert_same_route_tables().await;
    pair.assert_same_http("/user/%E7%A7%80%E6%A8%B9/tree?x=%2F")
        .await;
    pair.post_route(
        "/user/%E7%A7%80%E6%A8%B9",
        json!({
            "target": format!("{}/updated", pair.echo_target()),
            "jupyterhub": true,
            "user": "秀樹-updated"
        }),
    )
    .await;
    pair.assert_same_route_tables().await;
    pair.assert_same_http("/user/%E7%A7%80%E6%A8%B9/tree?x=%2F")
        .await;
    pair.delete_route("/user/%E7%A7%80%E6%A8%B9").await;
    pair.assert_same_route_tables().await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_longest_http_route_selection() {
    let pair = OraclePair::start().await;
    pair.post_route(
        "/selection",
        json!({ "target": format!("{}/broad", pair.echo_target()) }),
    )
    .await;
    pair.post_route(
        "/selection/specific",
        json!({ "target": format!("{}/specific", pair.echo_target()) }),
    )
    .await;
    pair.assert_same_http("/selection/specific/tree?x=%2F")
        .await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_every_path_option_combination() {
    for include_prefix in [false, true] {
        for prepend_path in [false, true] {
            let mut args = Vec::new();
            if !include_prefix {
                args.push("--no-include-prefix".to_owned());
            }
            if !prepend_path {
                args.push("--no-prepend-path".to_owned());
            }
            let pair = OraclePair::start_with_args(&args).await;
            pair.post_route(
                "/base",
                json!({ "target": format!("{}/target", pair.echo_target()) }),
            )
            .await;
            pair.assert_same_http("/base/tail?x=%2F&unicode=%E7%A7%80")
                .await;
        }
    }
}

#[tokio::test]
async fn chp_and_rust_agree_on_host_routing_and_redirect_policy() {
    let pair = OraclePair::start_with_args(&[
        "--host-routing".to_owned(),
        "--auto-rewrite".to_owned(),
        "--protocol-rewrite".to_owned(),
        "https".to_owned(),
    ])
    .await;
    pair.post_route(
        "/example.test/user",
        json!({ "target": pair.echo_target() }),
    )
    .await;
    pair.assert_same_http_with_host("/user/tree?x=%2F", "example.test")
        .await;
    pair.assert_same_http_with_host("/user/redirect", "example.test")
        .await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_health_errors_and_metric_schema() {
    let pair = OraclePair::start().await;
    pair.assert_same_http("/_chp_healthz").await;
    pair.assert_same_http("/missing?url=%2Fsemantic").await;

    let unavailable =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve unavailable target");
    let target = unavailable.local_addr().expect("unavailable address");
    drop(unavailable);
    pair.post_route(
        "/unavailable",
        json!({ "target": pair.host_target(&format!("http://{target}")) }),
    )
    .await;
    pair.assert_same_http("/unavailable").await;
    pair.assert_same_metrics().await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_custom_404_and_503_errors() {
    let errors = CustomErrorFixture::start().await;
    let pair = OraclePair::start_with_args(&["--error-target".to_owned(), errors.target()]).await;
    pair.assert_same_http("/missing?next=%2Ftree").await;

    let unavailable = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve custom-error unavailable target");
    let target = unavailable.local_addr().expect("unavailable address");
    drop(unavailable);
    pair.post_route(
        "/unavailable",
        json!({ "target": pair.host_target(&format!("http://{target}")) }),
    )
    .await;
    pair.assert_same_http("/unavailable?next=%2Ftree").await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_websocket_upgrade_and_messages() {
    let websocket = WebSocketFixture::start().await;
    let pair = OraclePair::start().await;
    pair.post_route(
        "/channels",
        json!({ "target": pair.host_target(&websocket.target()) }),
    )
    .await;
    pair.assert_same_websocket("/channels/kernel?token=a%2Fb", b"task11-websocket")
        .await;
    pair.assert_same_metrics().await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_public_tls() {
    let server = tls_server_identity();
    let server_cert = server.certificate.clone();
    let server_root = server.root.clone();
    let server_key = server.key.clone();
    let wrong_root = fixture_path("vendor/pingora-core-0.8.1/examples/keys/client-ca/cert.pem");
    let pair = OraclePair::start_with_args(&[
        "--ssl-cert".to_owned(),
        server_cert.display().to_string(),
        "--ssl-key".to_owned(),
        server_key.display().to_string(),
    ])
    .await;
    pair.assert_same_tls_health(&server_root, &wrong_root).await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_public_mutual_tls() {
    let server = tls_server_identity();
    let server_cert = server.certificate.clone();
    let server_root = server.root.clone();
    let server_key = server.key.clone();
    let client_ca = fixture_path("vendor/pingora-core-0.8.1/examples/keys/client-ca/cert.pem");
    let client_cert = fixture_path("vendor/pingora-core-0.8.1/examples/keys/clients/cert-1.pem");
    let client_key = fixture_path("vendor/pingora-core-0.8.1/examples/keys/clients/key-1.pem");
    let untrusted_client_cert =
        fixture_path("vendor/pingora-core-0.8.1/examples/keys/clients/invalid-cert.pem");
    let untrusted_client_key =
        fixture_path("vendor/pingora-core-0.8.1/examples/keys/clients/invalid-key.pem");
    let pair = OraclePair::start_with_args(&[
        "--ssl-cert".to_owned(),
        server_cert.display().to_string(),
        "--ssl-key".to_owned(),
        server_key.display().to_string(),
        "--ssl-ca".to_owned(),
        client_ca.display().to_string(),
        "--ssl-request-cert".to_owned(),
        "--ssl-reject-unauthorized".to_owned(),
    ])
    .await;
    pair.assert_same_mutual_tls_health(
        &server_root,
        &client_cert,
        &client_key,
        &untrusted_client_cert,
        &untrusted_client_key,
    )
    .await;
}

fn fixture_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

struct TlsServerIdentity {
    certificate: std::path::PathBuf,
    root: std::path::PathBuf,
    key: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

fn tls_server_identity() -> TlsServerIdentity {
    let target = fixture_path("target");
    std::fs::create_dir_all(&target).expect("create target directory for TLS fixture");
    let directory = tempfile::Builder::new()
        .prefix("chp-differential-tls-")
        .tempdir_in(target)
        .expect("create mounted TLS fixture directory");
    let ca_cert = directory.path().join("ca.pem");
    let ca_key = directory.path().join("ca.key");
    let server_csr = directory.path().join("server.csr");
    let server_cert = directory.path().join("server.pem");
    let server_key = directory.path().join("server.key");
    let extensions = directory.path().join("server.ext");
    std::fs::write(
        &extensions,
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:openrusty.org\n",
    )
    .expect("write TLS server extensions");
    for arguments in [
        vec![
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-subj",
            "/CN=Task 11 Test CA",
            "-keyout",
            ca_key.to_str().unwrap(),
            "-out",
            ca_cert.to_str().unwrap(),
        ],
        vec![
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=openrusty.org",
            "-keyout",
            server_key.to_str().unwrap(),
            "-out",
            server_csr.to_str().unwrap(),
        ],
        vec![
            "x509",
            "-req",
            "-in",
            server_csr.to_str().unwrap(),
            "-CA",
            ca_cert.to_str().unwrap(),
            "-CAkey",
            ca_key.to_str().unwrap(),
            "-CAcreateserial",
            "-days",
            "2",
            "-extfile",
            extensions.to_str().unwrap(),
            "-out",
            server_cert.to_str().unwrap(),
        ],
    ] {
        let output = std::process::Command::new("openssl")
            .args(arguments)
            .output()
            .expect("run openssl for TLS fixture");
        assert!(
            output.status.success(),
            "openssl fixture generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    TlsServerIdentity {
        certificate: server_cert,
        root: ca_cert,
        key: server_key,
        _directory: directory,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn chp_and_rust_agree_on_unix_public_api_and_metrics_sockets() {
    let pair = OraclePair::start_unix().await;
    pair.post_route("/unix", json!({ "target": pair.echo_target() }))
        .await;
    pair.assert_same_route_tables().await;
    pair.assert_same_unix_http("/unix/tree?x=%2F").await;
    pair.delete_route("/unix").await;
    pair.assert_same_route_tables().await;
    pair.assert_same_metrics().await;
}
