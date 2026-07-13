//! Testable counters shared by the management API and later Prometheus rendering.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const SUMMARY_QUANTILES: &[f64] = &[0.01, 0.05, 0.5, 0.9, 0.95, 0.99, 0.999];
const SUMMARY_WINDOW_CAPACITY: usize = 4096;

/// Point-in-time counter values, independent of the Task 11 rendering format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricsSnapshot {
    pub requests_api: BTreeMap<u16, u64>,
    pub requests_proxy: BTreeMap<u16, u64>,
    pub api_route_get: u64,
    pub api_route_add: u64,
    pub api_route_delete: u64,
    pub requests_ws: u64,
    pub requests_web: u64,
    pub activity_persistence_failures: u64,
    pub activity_dropped: u64,
}

/// Process metrics used by request handlers.
#[derive(Debug, Default)]
pub struct Metrics {
    requests_api: Mutex<BTreeMap<u16, u64>>,
    requests_proxy: Mutex<BTreeMap<u16, u64>>,
    api_route_get: AtomicU64,
    api_route_add: AtomicU64,
    api_route_delete: AtomicU64,
    find_target_for_req: Mutex<Summary>,
    last_activity_updating: Mutex<Summary>,
    requests_ws: AtomicU64,
    requests_web: AtomicU64,
    activity_persistence_failures: AtomicU64,
    activity_dropped: AtomicU64,
}

#[derive(Debug, Default)]
struct Summary {
    count: u64,
    sum: f64,
    recent: std::collections::VecDeque<f64>,
}

struct SummarySnapshot {
    count: u64,
    sum: f64,
    sorted_recent: Vec<f64>,
}

impl Summary {
    fn observe(&mut self, duration: Duration) {
        let value = duration.as_secs_f64();
        self.count = self.count.saturating_add(1);
        self.sum += value;
        if self.recent.len() == SUMMARY_WINDOW_CAPACITY {
            self.recent.pop_front();
        }
        self.recent.push_back(value);
    }

    fn snapshot(&self) -> SummarySnapshot {
        SummarySnapshot {
            count: self.count,
            sum: self.sum,
            sorted_recent: self.recent.iter().copied().collect(),
        }
    }
}

impl SummarySnapshot {
    fn sort(&mut self) {
        self.sorted_recent.sort_by(f64::total_cmp);
    }

    fn quantile(&self, quantile: f64) -> f64 {
        if self.sorted_recent.is_empty() {
            return 0.0;
        }
        let index = ((self.sorted_recent.len() - 1) as f64 * quantile).round() as usize;
        self.sorted_recent[index]
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_api_request(&self, status: u16) {
        let mut counts = self
            .requests_api
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counts.entry(status).or_default() += 1;
    }

    pub(crate) fn record_api_route_get(&self) {
        self.api_route_get.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_api_route_add(&self) {
        self.api_route_add.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_api_route_delete(&self) {
        self.api_route_delete.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_find_target(&self, duration: Duration) {
        self.find_target_for_req
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observe(duration);
    }

    pub(crate) fn record_last_activity_update(&self, duration: Duration) {
        self.last_activity_updating
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observe(duration);
    }

    pub(crate) fn record_ws_request(&self) {
        self.requests_ws.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_web_request(&self) {
        self.requests_web.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_proxy_request(&self, status: u16) {
        let mut counts = self
            .requests_proxy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counts.entry(status).or_default() += 1;
    }

    pub(crate) fn record_activity_persistence_failure(&self) {
        self.activity_persistence_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_activity_drop(&self) {
        self.activity_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Return stable values for contract tests and future metric rendering.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests_api: self
                .requests_api
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            requests_proxy: self
                .requests_proxy
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            api_route_get: self.api_route_get.load(Ordering::Relaxed),
            api_route_add: self.api_route_add.load(Ordering::Relaxed),
            api_route_delete: self.api_route_delete.load(Ordering::Relaxed),
            requests_ws: self.requests_ws.load(Ordering::Relaxed),
            requests_web: self.requests_web.load(Ordering::Relaxed),
            activity_persistence_failures: self
                .activity_persistence_failures
                .load(Ordering::Relaxed),
            activity_dropped: self.activity_dropped.load(Ordering::Relaxed),
        }
    }

    /// Render the CHP 5.3.0 metric families using its exact names and labels.
    pub fn render_prometheus(&self) -> String {
        let snapshot = self.snapshot();
        let mut find_target = self
            .find_target_for_req
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot();
        let mut last_activity = self
            .last_activity_updating
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot();
        find_target.sort();
        last_activity.sort();
        let mut body = String::new();
        render_counter(
            &mut body,
            "api_route_get",
            "Count of API route get requests",
            snapshot.api_route_get,
        );
        render_counter(
            &mut body,
            "api_route_add",
            "Count of API route add requests",
            snapshot.api_route_add,
        );
        render_counter(
            &mut body,
            "api_route_delete",
            "Count of API route delete requests",
            snapshot.api_route_delete,
        );
        render_summary(
            &mut body,
            "find_target_for_req",
            "Summary of find target requests",
            &find_target,
        );
        render_summary(
            &mut body,
            "last_activity_updating",
            "Summary of last activity updating requests",
            &last_activity,
        );
        render_counter(
            &mut body,
            "requests_ws",
            "Count of websocket requests",
            snapshot.requests_ws,
        );
        render_counter(
            &mut body,
            "requests_web",
            "Count of web requests",
            snapshot.requests_web,
        );
        render_status_counter(
            &mut body,
            "requests_proxy",
            "Count of proxy requests",
            &snapshot.requests_proxy,
        );
        render_status_counter(
            &mut body,
            "requests_api",
            "Count of API requests",
            &snapshot.requests_api,
        );
        body
    }
}

fn render_counter(body: &mut String, name: &str, help: &str, value: u64) {
    body.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n\n"
    ));
}

fn render_status_counter(body: &mut String, name: &str, help: &str, values: &BTreeMap<u16, u64>) {
    body.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
    for (status, value) in values {
        body.push_str(&format!("{name}{{status=\"{status}\"}} {value}\n"));
    }
    body.push('\n');
}

fn render_summary(body: &mut String, name: &str, help: &str, summary: &SummarySnapshot) {
    body.push_str(&format!("# HELP {name} {help}\n# TYPE {name} summary\n"));
    for quantile in SUMMARY_QUANTILES {
        body.push_str(&format!(
            "{name}{{quantile=\"{quantile}\"}} {}\n",
            summary.quantile(*quantile)
        ));
    }
    body.push_str(&format!(
        "{name}_sum {}\n{name}_count {}\n\n",
        summary.sum, summary.count
    ));
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    use super::Metrics;

    #[test]
    fn rendering_does_not_hold_one_summary_lock_while_waiting_for_another() {
        let metrics = Arc::new(Metrics::new());
        metrics.record_find_target(Duration::from_millis(10));
        metrics.record_last_activity_update(Duration::from_millis(20));

        let last_activity = metrics
            .last_activity_updating
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let rendering = Arc::clone(&metrics);
        let render_thread = std::thread::spawn(move || rendering.render_prometheus());

        let (recorded, observed) = mpsc::channel();
        let recording = Arc::clone(&metrics);
        let record_thread = std::thread::spawn(move || {
            recording.record_find_target(Duration::from_millis(30));
            recorded.send(()).expect("report completed observation");
        });
        let completed_without_other_summary = observed.recv_timeout(Duration::from_millis(200));

        drop(last_activity);
        let _ = render_thread.join().expect("renderer thread");
        record_thread.join().expect("observer thread");
        assert!(
            completed_without_other_summary.is_ok(),
            "rendering held the find-target mutex while blocked on last-activity"
        );
    }

    #[test]
    fn summary_rendering_uses_one_sorted_snapshot_with_exact_count_and_sum() {
        let metrics = Metrics::new();
        for millis in [40, 10, 30, 20] {
            metrics.record_find_target(Duration::from_millis(millis));
        }

        let body = metrics.render_prometheus();
        assert!(body.contains("find_target_for_req{quantile=\"0.01\"} 0.01\n"));
        assert!(body.contains("find_target_for_req{quantile=\"0.5\"} 0.03\n"));
        assert!(body.contains("find_target_for_req{quantile=\"0.999\"} 0.04\n"));
        assert!(body.contains("find_target_for_req_sum 0.1\n"));
        assert!(body.contains("find_target_for_req_count 4\n"));
    }
}
