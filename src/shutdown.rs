//! Process ownership and ordered graceful-shutdown coordination.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use thiserror::Error;

use crate::activity::ActivityWriter;
use crate::api_server::{wait_for_shutdown, ManagementLifecycle};
use crate::route_table::RouteRegistry;

pub const TERMINAL_MUTATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Atomic PID-file acquisition failures.
#[derive(Debug, Error)]
pub enum PidFileError {
    #[error("PID file could not be created atomically")]
    Create(#[source] io::Error),
    #[error("PID file could not be written")]
    Write(#[source] io::Error),
}

/// RAII ownership of a PID file created with `create_new`.
pub struct PidFileGuard {
    path: PathBuf,
    identity: FileIdentity,
}

impl PidFileGuard {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, PidFileError> {
        let path = path.as_ref().to_owned();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(PidFileError::Create)?;
        let identity = FileIdentity::from_file(&file).map_err(PidFileError::Create)?;
        let guard = Self { path, identity };
        writeln!(file, "{}", std::process::id()).map_err(PidFileError::Write)?;
        file.sync_data().map_err(PidFileError::Write)?;
        Ok(guard)
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if self.identity.matches(&metadata) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    length: u64,
}

impl FileIdentity {
    fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                length: metadata.len(),
            })
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.dev() == self.device && metadata.ino() == self.inode
        }
        #[cfg(not(unix))]
        {
            metadata.is_file() && metadata.len() >= self.length
        }
    }
}

/// Stops management admission, drains registry work, then flushes activity.
pub struct ShutdownCoordinator {
    management: Arc<ManagementLifecycle>,
    registry: Arc<RouteRegistry>,
    activity: ActivityWriter,
    mutation_timeout: Duration,
}

impl ShutdownCoordinator {
    pub fn new(
        management: Arc<ManagementLifecycle>,
        registry: Arc<RouteRegistry>,
        activity: ActivityWriter,
        mutation_timeout: Duration,
    ) -> Self {
        Self {
            management,
            registry,
            activity,
            mutation_timeout,
        }
    }
}

#[async_trait]
impl BackgroundService for ShutdownCoordinator {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        wait_for_shutdown(&mut shutdown).await;
        self.management.wait_for_accepts_stopped().await;

        let outcome = self.registry.drain_mutations(self.mutation_timeout).await;
        if outcome.timed_out {
            tracing::error!(
                active_mutations = outcome.active_mutations,
                timeout_ms = self.mutation_timeout.as_millis(),
                "terminal route mutation drain timed out"
            );
        }
        for failure in outcome.detached_failures {
            tracing::error!(
                operation = ?failure.operation,
                "detached route mutation failed"
            );
        }
        for panic in outcome.detached_panics {
            tracing::error!(
                operation = ?panic.operation,
                message = panic.message,
                "detached route mutation panicked"
            );
        }
        if outcome.dropped_detached_failures != 0 || outcome.dropped_detached_panics != 0 {
            tracing::error!(
                dropped_failures = outcome.dropped_detached_failures,
                dropped_panics = outcome.dropped_detached_panics,
                "route mutation shutdown diagnostics overflowed"
            );
        }

        self.activity.flush().await;
        tracing::info!("management mutations and activity persistence drained");
    }
}
