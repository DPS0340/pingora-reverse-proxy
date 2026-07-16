//! Process ownership and ordered graceful-shutdown coordination.

use std::io::{self, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use thiserror::Error;

use crate::activity::ActivityWriter;
use crate::api_server::{wait_for_shutdown, ManagementLifecycle, TrafficLifecycle};
use crate::path_ownership::OwnedPath;
use crate::route_table::RouteRegistry;

pub const TERMINAL_MUTATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
pub const ACCEPT_STOP_TIMEOUT: Duration = Duration::from_secs(1);
pub const ACTIVITY_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
pub const TRAFFIC_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Shutdown phases run in this order. Management and public accept-stop
/// acknowledgements share one concurrent deadline in the first phase.
pub const ORDERED_SHUTDOWN_PHASE_DEADLINES: [(&str, Duration); 4] = [
    ("management and public accept-stop", ACCEPT_STOP_TIMEOUT),
    ("terminal mutation drain", TERMINAL_MUTATION_DRAIN_TIMEOUT),
    ("activity watermark flush", ACTIVITY_FLUSH_TIMEOUT),
    ("admitted traffic drain", TRAFFIC_DRAIN_TIMEOUT),
];
/// Pingora's timer strictly contains every ordered lifecycle phase plus one safety second.
pub const SHUTDOWN_GRACE_PERIOD_SECONDS: u64 = ACCEPT_STOP_TIMEOUT.as_secs()
    + TERMINAL_MUTATION_DRAIN_TIMEOUT.as_secs()
    + ACTIVITY_FLUSH_TIMEOUT.as_secs()
    + TRAFFIC_DRAIN_TIMEOUT.as_secs()
    + 1;
/// Once the grace phase finishes, bound final Tokio runtime teardown separately.
pub const RUNTIME_SHUTDOWN_TIMEOUT_SECONDS: u64 = 1;

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
    // Struct fields drop in declaration order. Keep the descriptor after the path owner so
    // cleanup compares and quarantines the path while the original inode is still pinned.
    _owned: OwnedPath,
    _file: std::fs::File,
}

impl PidFileGuard {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, PidFileError> {
        let path = path.as_ref().to_owned();
        let (file, owned) = OwnedPath::create_new_file(path).map_err(PidFileError::Create)?;
        let mut guard = Self {
            _owned: owned,
            _file: file,
        };
        writeln!(guard._file, "{}", std::process::id()).map_err(PidFileError::Write)?;
        guard._file.sync_data().map_err(PidFileError::Write)?;
        Ok(guard)
    }
}

impl PidFileGuard {
    #[cfg(test)]
    fn cleanup_for_test<B, V, R>(
        &mut self,
        before_quarantine: B,
        after_verify: V,
        before_restore: R,
    ) -> Option<PathBuf>
    where
        B: FnOnce(),
        V: FnOnce(),
        R: FnOnce(),
    {
        self._owned
            .cleanup_with_hooks(before_quarantine, after_verify, before_restore)
    }
}

/// Stops management admission, drains registry work, then flushes activity.
pub struct ShutdownCoordinator {
    management: Arc<ManagementLifecycle>,
    traffic: Arc<TrafficLifecycle>,
    registry: Arc<RouteRegistry>,
    activity: ActivityWriter,
    mutation_timeout: Duration,
}

impl ShutdownCoordinator {
    pub fn new(
        management: Arc<ManagementLifecycle>,
        traffic: Arc<TrafficLifecycle>,
        registry: Arc<RouteRegistry>,
        activity: ActivityWriter,
        mutation_timeout: Duration,
    ) -> Self {
        Self {
            management,
            traffic,
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
        if tokio::time::timeout(ACCEPT_STOP_TIMEOUT, async {
            tokio::join!(
                self.management.wait_for_accepts_stopped(),
                self.traffic.wait_for_accepts_stopped()
            );
        })
        .await
        .is_err()
        {
            tracing::error!(
                timeout_ms = ACCEPT_STOP_TIMEOUT.as_millis(),
                "listener accept-stop acknowledgement timed out"
            );
        }

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

        if tokio::time::timeout(ACTIVITY_FLUSH_TIMEOUT, self.activity.flush())
            .await
            .is_err()
        {
            tracing::error!(
                timeout_ms = ACTIVITY_FLUSH_TIMEOUT.as_millis(),
                "activity acceptance watermark flush timed out"
            );
        }

        if tokio::time::timeout(TRAFFIC_DRAIN_TIMEOUT, self.traffic.wait_for_drain())
            .await
            .is_err()
        {
            tracing::error!(
                active_traffic = self.traffic.active(),
                timeout_ms = TRAFFIC_DRAIN_TIMEOUT.as_millis(),
                "admitted HTTP/WebSocket drain timed out"
            );
        }
        tracing::info!("ordered shutdown lifecycle completed");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PidFileGuard, ACCEPT_STOP_TIMEOUT, ORDERED_SHUTDOWN_PHASE_DEADLINES,
        SHUTDOWN_GRACE_PERIOD_SECONDS, TERMINAL_MUTATION_DRAIN_TIMEOUT,
    };
    use std::fs;
    use std::time::Duration;

    #[test]
    fn ordered_shutdown_deadlines_restore_the_five_second_terminal_bound() {
        assert_eq!(TERMINAL_MUTATION_DRAIN_TIMEOUT, Duration::from_secs(5));
        assert_eq!(
            ORDERED_SHUTDOWN_PHASE_DEADLINES,
            [
                ("management and public accept-stop", ACCEPT_STOP_TIMEOUT),
                ("terminal mutation drain", Duration::from_secs(5)),
                ("activity watermark flush", Duration::from_secs(1)),
                ("admitted traffic drain", Duration::from_secs(2)),
            ]
        );
        let sequential_bound: Duration = ORDERED_SHUTDOWN_PHASE_DEADLINES
            .iter()
            .map(|(_, deadline)| *deadline)
            .sum();
        assert!(
            Duration::from_secs(SHUTDOWN_GRACE_PERIOD_SECONDS) > sequential_bound,
            "Pingora grace must strictly contain every sequential phase deadline"
        );
    }

    #[test]
    fn pid_cleanup_never_deletes_a_boundary_replacement() {
        let directory = tempfile::tempdir().expect("temporary PID ownership directory");
        let path = directory.path().join("proxy.pid");
        let mut guard = PidFileGuard::acquire(&path).expect("acquire PID file");

        guard.cleanup_for_test(
            || {
                fs::remove_file(&path).expect("replace owned PID");
                fs::write(&path, b"replacement\n").expect("write replacement PID");
            },
            || {},
            || {},
        );

        assert_eq!(
            fs::read(&path).expect("replacement PID was preserved"),
            b"replacement\n"
        );
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("list PID directory")
                .count(),
            1
        );
    }

    #[test]
    fn pid_cleanup_preserves_quarantine_when_restore_collides() {
        let directory = tempfile::tempdir().expect("temporary PID ownership directory");
        let path = directory.path().join("proxy.pid");
        let mut guard = PidFileGuard::acquire(&path).expect("acquire PID file");

        let quarantine = guard.cleanup_for_test(
            || {
                fs::remove_file(&path).expect("replace owned PID");
                fs::write(&path, b"replacement\n").expect("write replacement PID");
            },
            || {},
            || fs::write(&path, b"restore-collision\n").expect("install restore collision"),
        );

        assert_eq!(
            fs::read(&path).expect("collision PID was preserved"),
            b"restore-collision\n"
        );
        let quarantine = quarantine.expect("foreign replacement must remain quarantined");
        assert_eq!(
            fs::read(quarantine).expect("quarantined PID was preserved"),
            b"replacement\n"
        );
    }

    #[test]
    fn ordinary_owned_pid_is_removed_without_private_debris() {
        let directory = tempfile::tempdir().expect("temporary PID ownership directory");
        let path = directory.path().join("proxy.pid");
        let guard = PidFileGuard::acquire(&path).expect("acquire PID file");

        drop(guard);

        assert!(!path.exists());
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("list PID directory")
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_pid_parent_is_rejected_before_file_creation() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary unsafe PID directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777))
            .expect("make PID directory unsafe");
        let path = directory.path().join("proxy.pid");

        assert!(PidFileGuard::acquire(&path).is_err());
        assert!(!path.exists(), "unsafe parent received a PID file");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
