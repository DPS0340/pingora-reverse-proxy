use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use pingora::server::Server;
use pingora_reverse_proxy::activity::ActivityWriter;
use pingora_reverse_proxy::config::{AppConfig, Cli, ConfigError, ListenerConfig, StoreConfig};
use pingora_reverse_proxy::metrics::Metrics;
use pingora_reverse_proxy::proxy::{ChpProxy, ProxyBuildError};
use pingora_reverse_proxy::route_table::RouteRegistry;
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::{Store, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
enum StartupError {
    #[error(transparent)]
    Cli(#[from] clap::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Proxy(#[from] ProxyBuildError),
    #[error("failed to initialize route storage")]
    Store(#[source] StoreError),
    #[error("failed to initialize the async runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to initialize Pingora: {0}")]
    Pingora(String),
    #[error("storage backend {0:?} is not implemented yet")]
    UnsupportedStore(StoreConfig),
    #[error("public TLS and Unix listeners are implemented in Task 8")]
    UnsupportedListener,
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
    if config.store != StoreConfig::Memory {
        return Err(StartupError::UnsupportedStore(config.store));
    }
    if config.public_tls.is_some() || !matches!(config.public_listener, ListenerConfig::Tcp(_)) {
        return Err(StartupError::UnsupportedListener);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let registry = runtime
        .block_on(RouteRegistry::load(store))
        .map_err(StartupError::Store)?;
    let metrics = Arc::new(Metrics::new());
    let activity = runtime
        .block_on(async { ActivityWriter::start_with_metrics(Arc::clone(&registry), 64, metrics) });
    let proxy = runtime.block_on(ChpProxy::from_config(registry, &config, activity))?;

    let mut server = Server::new(None).map_err(|error| StartupError::Pingora(error.to_string()))?;
    server.bootstrap();
    let mut service = pingora::proxy::http_proxy_service(&server.configuration, proxy);
    if let ListenerConfig::Tcp(address) = &config.public_listener {
        service.add_tcp(address);
    }
    server.add_service(service);
    server.run_forever()
}
