use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use pingora::server::Server;
use pingora::services::background::background_service;
use pingora_reverse_proxy::activity::ActivityWriter;
use pingora_reverse_proxy::api::{router as api_router, ApiState};
use pingora_reverse_proxy::api_server::{ApiServer, ManagementLifecycle, PreboundPublicService};
use pingora_reverse_proxy::config::{AppConfig, Cli};
use pingora_reverse_proxy::metrics::Metrics;
use pingora_reverse_proxy::proxy::ChpProxy;
use pingora_reverse_proxy::route_table::RouteRegistry;
use pingora_reverse_proxy::shutdown::{
    ShutdownCoordinator, RUNTIME_SHUTDOWN_TIMEOUT_SECONDS, SHUTDOWN_GRACE_PERIOD_SECONDS,
    TERMINAL_MUTATION_DRAIN_TIMEOUT,
};
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::redis::{RedisStore, RedisStoreConfig};
use pingora_reverse_proxy::store::Store;
use serde_json::Value;

const SUMMARY_PREFIX: &str = "JUPYTERHUB_E2E_SUMMARY=";
const REQUIRED_SCENARIOS: &[&str] = &[
    "pinned_python_runtime",
    "external_proxy_configuration",
    "proxy_api_add_get_delete",
    "hub_root_route",
    "hub_user_api_add_get",
    "escaped_route_river",
    "escaped_route_unicode",
    "escaped_route_at_sign",
    "escaped_route_space",
    "login",
    "single_user_page",
    "kernel_websocket_message_flow",
    "hub_restart_existing_route_usable",
    "proxy_restart_backend_state",
    "proxy_route_reconciliation",
    "hub_user_api_delete",
    "host_routing",
    "run_owned_cleanup",
];

fn redact(mut text: String) -> String {
    for variable in [
        "CONFIGPROXY_AUTH_TOKEN",
        "JUPYTERHUB_E2E_API_TOKEN",
        "JUPYTERHUB_E2E_LOGIN_PASSWORD",
    ] {
        if let Ok(secret) = std::env::var(variable) {
            if !secret.is_empty() {
                text = text.replace(&secret, "<redacted>");
            }
        }
    }
    text
}

#[test]
fn real_jupyterhub_5_5_0_external_proxy_end_to_end() {
    if std::env::var("JUPYTERHUB_E2E_RUN").as_deref() != Ok("1") {
        eprintln!("JupyterHub E2E is exercised by `just test-jupyterhub`");
        return;
    }

    let backend = std::env::var("STORE_BACKEND").expect("STORE_BACKEND is required");
    assert!(matches!(backend.as_str(), "memory" | "redis"));
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/jupyterhub-e2e.py");
    let output = Command::new("python3")
        .args([
            script.to_str().expect("script path is UTF-8"),
            "--scenario",
            "--proxy-helper",
            std::env::current_exe()
                .expect("current test executable")
                .to_str()
                .expect("test executable path is UTF-8"),
        ])
        .env("PYTHONUNBUFFERED", "1")
        .output()
        .expect("launch JupyterHub 5.5.0 E2E scenario");

    let stdout = redact(String::from_utf8_lossy(&output.stdout).into_owned());
    let stderr = redact(String::from_utf8_lossy(&output.stderr).into_owned());
    assert!(
        output.status.success(),
        "JupyterHub E2E failed for {backend}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let summary_line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with(SUMMARY_PREFIX))
        .expect("E2E emitted its scenario summary");
    let summary: Value = serde_json::from_str(&summary_line[SUMMARY_PREFIX.len()..])
        .expect("scenario summary is JSON");
    assert_eq!(summary["backend"], backend);
    assert_eq!(summary["jupyterhub"], "5.5.0");
    let actual: BTreeSet<&str> = summary["scenarios"]
        .as_array()
        .expect("summary scenarios are an array")
        .iter()
        .map(|scenario| scenario.as_str().expect("scenario name is a string"))
        .collect();
    let required: BTreeSet<&str> = REQUIRED_SCENARIOS.iter().copied().collect();
    assert_eq!(actual, required, "the real E2E must cover every scenario");
}

#[test]
fn jupyterhub_proxy_helper() {
    if std::env::var("JUPYTERHUB_PROXY_HELPER").as_deref() != Ok("1") {
        return;
    }
    run_proxy_helper().expect("run real external proxy helper");
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required by the proxy helper"))
}

fn run_proxy_helper() -> Result<(), Box<dyn std::error::Error>> {
    let backend = required_env("STORE_BACKEND");
    let public_port = required_env("JUPYTERHUB_PROXY_PORT");
    let api_port = required_env("JUPYTERHUB_PROXY_API_PORT");
    let mut arguments = vec![
        "jupyterhub-e2e-proxy".to_owned(),
        "--ip".to_owned(),
        "127.0.0.1".to_owned(),
        "--port".to_owned(),
        public_port,
        "--api-ip".to_owned(),
        "127.0.0.1".to_owned(),
        "--api-port".to_owned(),
        api_port,
        "--storage-backend".to_owned(),
        backend.clone(),
    ];
    if std::env::var("JUPYTERHUB_HOST_ROUTING").as_deref() == Ok("1") {
        arguments.push("--host-routing".to_owned());
    }
    let config = AppConfig::try_from(Cli::try_parse_from(arguments)?)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let store: Arc<dyn Store> = match backend.as_str() {
        "memory" => Arc::new(MemoryStore::new()),
        "redis" => Arc::new(
            runtime.block_on(RedisStore::connect(
                RedisStoreConfig::new(required_env("TEST_REDIS_URL"))
                    .with_key(required_env("JUPYTERHUB_REDIS_ROUTE_KEY")),
            ))?,
        ),
        _ => return Err(format!("unsupported E2E backend {backend:?}").into()),
    };
    let registry = runtime.block_on(RouteRegistry::load(store))?;
    let metrics = Arc::new(Metrics::new());
    let activity = runtime.block_on(async {
        ActivityWriter::start_with_metrics(Arc::clone(&registry), 64, Arc::clone(&metrics))
    });
    let proxy = runtime.block_on(ChpProxy::from_config(
        Arc::clone(&registry),
        &config,
        activity.clone(),
    ))?;
    let traffic = proxy.traffic_lifecycle();
    let management = Arc::new(ManagementLifecycle::default());
    let api_state = ApiState::new(
        Arc::clone(&registry),
        config.auth_token.as_deref(),
        Arc::clone(&metrics),
    );
    let api_server = runtime.block_on(ApiServer::bind(
        api_router(api_state),
        config.api_listener.clone(),
        config.api_tls.clone(),
        Some(Arc::clone(&management)),
    ))?;

    let mut server = Server::new(None).map_err(|error| error.to_string())?;
    if let Some(configuration) = Arc::get_mut(&mut server.configuration) {
        configuration.grace_period_seconds = Some(SHUTDOWN_GRACE_PERIOD_SECONDS);
        configuration.graceful_shutdown_timeout_seconds = Some(RUNTIME_SHUTDOWN_TIMEOUT_SECONDS);
    }
    server.bootstrap();
    let public = pingora::proxy::http_proxy_service(&server.configuration, proxy);
    let public_failed = Arc::new(AtomicBool::new(false));
    let public_handle = server.add_service(PreboundPublicService::new(
        public,
        &config.public_listener,
        config.public_tls.clone(),
        Arc::clone(&traffic),
        Arc::clone(&public_failed),
    )?);
    let api_handle = server.add_service(background_service("CHP management API", api_server));
    api_handle.add_dependency(&public_handle);
    server.add_service(background_service(
        "CHP graceful shutdown",
        ShutdownCoordinator::new(
            management,
            traffic,
            registry,
            activity,
            TERMINAL_MUTATION_DRAIN_TIMEOUT,
        ),
    ));
    server.run(Default::default());
    if public_failed.load(Ordering::Acquire) {
        return Err("public proxy service failed".into());
    }
    Ok(())
}
