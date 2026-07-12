use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use pingora::server::Server;
use pingora::services::background::background_service;
use pingora_reverse_proxy::activity::ActivityWriter;
use pingora_reverse_proxy::api::{router as api_router, ApiState};
#[cfg(unix)]
use pingora_reverse_proxy::api_server::UnixSocketCleanup;
use pingora_reverse_proxy::api_server::{
    ensure_socket_path_available, install_listener, metrics_router, redirect_router, ApiServer,
    ListenerError, ManagementLifecycle,
};
use pingora_reverse_proxy::config::{
    AppConfig, Cli, ConfigError, ListenerConfig, LogLevel, StoreConfig,
};
use pingora_reverse_proxy::metrics::Metrics;
use pingora_reverse_proxy::proxy::{ChpProxy, ProxyBuildError};
use pingora_reverse_proxy::route_table::{
    install_route_mutation_panic_hook_at_startup, RouteRegistry,
};
use pingora_reverse_proxy::shutdown::{
    PidFileError, PidFileGuard, ShutdownCoordinator, TERMINAL_MUTATION_DRAIN_TIMEOUT,
};
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::{Store, StoreError};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Error)]
enum StartupError {
    #[error(transparent)]
    Cli(#[from] clap::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Proxy(#[from] ProxyBuildError),
    #[error(transparent)]
    Listener(#[from] ListenerError),
    #[error(transparent)]
    PidFile(#[from] PidFileError),
    #[error("failed to initialize route storage")]
    Store(#[source] StoreError),
    #[error("failed to initialize the async runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to initialize Pingora: {0}")]
    Pingora(String),
    #[error("storage backend {0:?} is not implemented yet")]
    UnsupportedStore(StoreConfig),
    #[error("redirect listener requires a TCP public listener")]
    InvalidRedirectListener,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(StartupError::Cli(error)) => {
            let exit_code = error.exit_code();
            let _ = error.print();
            ExitCode::from(u8::try_from(exit_code).unwrap_or(1))
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), StartupError> {
    let cli = Cli::try_parse()?;
    let config = AppConfig::try_from(cli)?;
    initialize_logging(config.log_level);
    // Crash/logging ownership must be final before the mutation redaction hook.
    install_route_mutation_panic_hook_at_startup();

    if config.store != StoreConfig::Memory {
        return Err(StartupError::UnsupportedStore(config.store));
    }
    if let ListenerConfig::Unix(path) = &config.public_listener {
        ensure_socket_path_available(path)?;
    }
    let _pid_file = config
        .pid_file
        .as_ref()
        .map(PidFileGuard::acquire)
        .transpose()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let registry = runtime
        .block_on(RouteRegistry::load(store))
        .map_err(StartupError::Store)?;
    let metrics = Arc::new(Metrics::new());
    let activity = runtime.block_on(async {
        ActivityWriter::start_with_metrics(Arc::clone(&registry), 64, Arc::clone(&metrics))
    });
    let proxy = runtime.block_on(ChpProxy::from_config(
        Arc::clone(&registry),
        &config,
        activity.clone(),
    ))?;

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
    let metrics_server = config
        .metrics_listener
        .clone()
        .map(|listener| {
            runtime.block_on(ApiServer::bind(
                metrics_router(Arc::clone(&metrics)),
                listener,
                None,
                None,
            ))
        })
        .transpose()?;
    let redirect_server = config
        .redirect_port
        .map(|port| {
            let (listener, https_port) = redirect_configuration(&config, port)?;
            runtime
                .block_on(ApiServer::bind(
                    redirect_router(https_port),
                    listener,
                    None,
                    None,
                ))
                .map_err(StartupError::Listener)
        })
        .transpose()?;

    let mut server = Server::new(None).map_err(|error| StartupError::Pingora(error.to_string()))?;
    if let Some(configuration) = Arc::get_mut(&mut server.configuration) {
        configuration.grace_period_seconds = Some(0);
        configuration.graceful_shutdown_timeout_seconds = Some(1);
    }
    server.bootstrap();

    let mut public = pingora::proxy::http_proxy_service(&server.configuration, proxy);
    install_listener(
        &mut public,
        config.public_listener.clone(),
        config.public_tls.clone(),
    )?;
    server.add_service(public);
    #[cfg(unix)]
    if let ListenerConfig::Unix(path) = &config.public_listener {
        server.add_service(background_service(
            "CHP public UDS cleanup",
            UnixSocketCleanup::new(path.clone()),
        ));
    }
    server.add_service(background_service("CHP management API", api_server));
    if let Some(metrics_server) = metrics_server {
        server.add_service(background_service("CHP metrics", metrics_server));
    }
    if let Some(redirect_server) = redirect_server {
        server.add_service(background_service("CHP HTTPS redirect", redirect_server));
    }
    server.add_service(background_service(
        "CHP graceful shutdown",
        ShutdownCoordinator::new(
            management,
            registry,
            activity,
            TERMINAL_MUTATION_DRAIN_TIMEOUT,
        ),
    ));
    server.run(Default::default());
    Ok(())
}

fn initialize_logging(level: LogLevel) {
    let directive = match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(directive))
        .try_init();
}

fn redirect_configuration(
    config: &AppConfig,
    redirect_port: u16,
) -> Result<(ListenerConfig, u16), StartupError> {
    let ListenerConfig::Tcp(public) = &config.public_listener else {
        return Err(StartupError::InvalidRedirectListener);
    };
    let public_address = public
        .parse::<SocketAddr>()
        .map_err(|_| StartupError::InvalidRedirectListener)?;
    let https_port = config.redirect_to.unwrap_or(public_address.port());
    Ok((
        ListenerConfig::Tcp(SocketAddr::new(public_address.ip(), redirect_port).to_string()),
        https_port,
    ))
}
