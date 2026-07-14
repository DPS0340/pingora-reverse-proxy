use std::path::PathBuf;
use std::process::Command;

#[test]
fn canonical_harness_builds_and_launches_the_shipped_binary() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let compose = std::fs::read_to_string(workspace.join("compose.test.yml"))
        .expect("read Compose test definition");
    let harness = std::fs::read_to_string(workspace.join("scripts/jupyterhub-e2e.py"))
        .expect("read JupyterHub harness");

    assert!(
        compose.contains("cargo build --locked"),
        "the E2E image must build the deployable binary with the lockfile"
    );
    assert!(
        compose.contains("/usr/local/bin/pingora-reverse-proxy"),
        "the E2E image must install the deployable binary"
    );
    assert!(
        !compose.contains("jupyterhub-e2e-test"),
        "the E2E image must not execute a test-only proxy helper"
    );
    assert!(
        harness.contains("--proxy-binary"),
        "the scenario must receive the shipped binary explicitly"
    );
    assert!(
        !harness.contains("jupyterhub_proxy_helper"),
        "the scenario must not launch the manually assembled helper"
    );
}

#[test]
fn sidecar_runtime_selection_remains_explicitly_fail_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"))
        .args(["--storage-backend", "sidecar"])
        .output()
        .expect("launch the deployable binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sidecar runtime wiring remains fail-closed for Task 13"));
}
