use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "support/sidecar.rs"]
#[allow(dead_code)]
mod sidecar_support;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipped_binary_loads_authenticated_sidecar_before_listener_readiness() {
    const TOKEN: &str = "SIDECAR_RUNTIME_TOKEN_SENTINEL_8246";
    let fixture = sidecar_support::SidecarFixture::start_with_token(Some(TOKEN)).await;
    let snapshot_gate = sidecar_support::FaultGate::new();
    fixture.block_snapshot(snapshot_gate.clone()).await;
    let public_reservation =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve public port");
    let api_reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve API port");
    let public = public_reservation
        .local_addr()
        .expect("public address")
        .port();
    let api = api_reservation.local_addr().expect("API address").port();
    assert_ne!(public, api);
    drop((public_reservation, api_reservation));
    let child = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"))
        .args([
            "--storage-backend",
            "sidecar",
            "--ip",
            "127.0.0.1",
            "--port",
            &public.to_string(),
            "--api-ip",
            "127.0.0.1",
            "--api-port",
            &api.to_string(),
        ])
        .env("PINGORA_SIDECAR_URL", fixture.base_url())
        .env("PINGORA_SIDECAR_BEARER_TOKEN", TOKEN)
        .env("PINGORA_SIDECAR_CONNECT_TIMEOUT_MS", "250")
        .env("PINGORA_SIDECAR_REQUEST_TIMEOUT_MS", "500")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch shipped binary");
    let mut child = ChildGuard(child);
    tokio::time::timeout(Duration::from_secs(3), snapshot_gate.wait_until_arrived())
        .await
        .expect("startup snapshot request did not reach the explicit barrier");
    for port in [public, api] {
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "listener {port} accepted a connection before snapshot release"
        );
    }
    assert!(
        child.0.try_wait().expect("poll blocked binary").is_none(),
        "shipped binary exited while the snapshot was blocked"
    );
    snapshot_gate.release();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .expect("probe client");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            child.0.try_wait().expect("poll shipped binary").is_none(),
            "shipped binary exited before sidecar-backed readiness"
        );
        let health = client
            .get(format!("http://127.0.0.1:{public}/_chp_healthz"))
            .send()
            .await;
        let routes = client
            .get(format!("http://127.0.0.1:{api}/api/routes"))
            .send()
            .await;
        if health.is_ok_and(|response| response.status().is_success())
            && routes.is_ok_and(|response| response.status().is_success())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sidecar-backed readiness timed out"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(fixture.request_count(sidecar_support::HEALTH).await, 1);
    assert_eq!(fixture.request_count(sidecar_support::SNAPSHOT).await, 1);

    unsafe {
        libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().expect("poll graceful exit") {
            assert!(
                status.success(),
                "sidecar-backed binary did not exit cleanly"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sidecar-backed shutdown timed out"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
fn shipped_binary_required_auth_policy_fails_before_listener_binding() {
    let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve API port");
    let api = reservation.local_addr().expect("API address").port();
    drop(reservation);
    let stderr = tempfile::NamedTempFile::new().expect("create stderr capture");
    let child = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"))
        .args(["--api-ip", "127.0.0.1", "--api-port", &api.to_string()])
        .env("PINGORA_REQUIRE_AUTH_TOKEN", "true")
        .env("CONFIGPROXY_AUTH_TOKEN", "")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr.reopen().expect("reopen stderr capture"))
        .spawn()
        .expect("launch shipped binary");
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll required-auth startup") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "binary did not reject the empty required auth token"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let diagnostic = std::fs::read_to_string(stderr.path()).expect("read startup diagnostic");

    assert!(!status.success(), "empty required auth token was accepted");
    assert!(
        diagnostic.contains("required management authentication token is missing or empty"),
        "unexpected startup diagnostic: {diagnostic}"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", api)).is_err(),
        "API listener bound despite required-auth validation failure"
    );
}
