use std::collections::BTreeMap;
use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::extract::Request as AxumRequest;
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use http::header::{CONTENT_TYPE, HOST};
use http::StatusCode;

use serde_json::Value;

const VOLATILE_HEADERS: &[&str] = &[
    "date",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
];

const SUMMARY_QUANTILES: &[&str] = &["0.01", "0.05", "0.5", "0.9", "0.95", "0.99", "0.999"];

const METRIC_FAMILIES: &[(&str, &str, &str)] = &[
    (
        "api_route_get",
        "counter",
        "Count of API route get requests",
    ),
    (
        "api_route_add",
        "counter",
        "Count of API route add requests",
    ),
    (
        "api_route_delete",
        "counter",
        "Count of API route delete requests",
    ),
    (
        "find_target_for_req",
        "summary",
        "Summary of find target requests",
    ),
    (
        "last_activity_updating",
        "summary",
        "Summary of last activity updating requests",
    ),
    ("requests_ws", "counter", "Count of websocket requests"),
    ("requests_web", "counter", "Count of web requests"),
    ("requests_proxy", "counter", "Count of proxy requests"),
    ("requests_api", "counter", "Count of API requests"),
];

const CHP_RUNTIME_METRIC_FAMILIES: &[&str] = &[
    "process_cpu_user_seconds_total",
    "process_cpu_system_seconds_total",
    "process_cpu_seconds_total",
    "process_start_time_seconds",
    "process_resident_memory_bytes",
    "process_virtual_memory_bytes",
    "process_heap_bytes",
    "process_open_fds",
    "process_max_fds",
    "nodejs_eventloop_lag_seconds",
    "nodejs_eventloop_lag_min_seconds",
    "nodejs_eventloop_lag_max_seconds",
    "nodejs_eventloop_lag_mean_seconds",
    "nodejs_eventloop_lag_stddev_seconds",
    "nodejs_eventloop_lag_p50_seconds",
    "nodejs_eventloop_lag_p90_seconds",
    "nodejs_eventloop_lag_p99_seconds",
    "nodejs_active_resources",
    "nodejs_active_resources_total",
    "nodejs_active_handles",
    "nodejs_active_handles_total",
    "nodejs_active_requests",
    "nodejs_active_requests_total",
    "nodejs_heap_size_total_bytes",
    "nodejs_heap_size_used_bytes",
    "nodejs_external_memory_bytes",
    "nodejs_heap_space_size_total_bytes",
    "nodejs_heap_space_size_used_bytes",
    "nodejs_heap_space_size_available_bytes",
    "nodejs_version_info",
    "nodejs_gc_duration_seconds",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservationSide {
    Chp,
    Rust,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EndpointMapping {
    role: String,
    chp: String,
    rust: String,
}

impl EndpointMapping {
    pub(crate) fn new(role: &str, chp: &str, rust: &str) -> Self {
        Self {
            role: role.to_owned(),
            chp: chp.to_owned(),
            rust: rust.to_owned(),
        }
    }

    fn address(&self, side: ObservationSide) -> &str {
        match side {
            ObservationSide::Chp => &self.chp,
            ObservationSide::Rust => &self.rust,
        }
    }

    fn token(&self) -> String {
        format!("<{}>", self.role)
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ObservedBody {
    Json(Value),
    Bytes(Vec<u8>),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct HttpObservation {
    status: u16,
    headers: BTreeMap<String, Vec<Vec<u8>>>,
    body: ObservedBody,
}

pub(crate) fn observation_for_test(
    side: ObservationSide,
    status: u16,
    headers: &[(&str, &[u8])],
    body: &[u8],
    mappings: &[EndpointMapping],
) -> HttpObservation {
    observation_from_parts(
        side,
        status,
        headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_vec())),
        body,
        mappings,
    )
}

fn observation_from_parts(
    side: ObservationSide,
    status: u16,
    headers: impl IntoIterator<Item = (String, Vec<u8>)>,
    body: &[u8],
    mappings: &[EndpointMapping],
) -> HttpObservation {
    let mut semantic_headers: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
    for (name, mut value) in headers {
        let name = name.to_ascii_lowercase();
        if VOLATILE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if name == "location" {
            value = normalize_structural_url_bytes(&value, side, mappings);
        }
        semantic_headers.entry(name).or_default().push(value);
    }
    for values in semantic_headers.values_mut() {
        values.sort();
    }

    let body = match serde_json::from_slice(body) {
        Ok(mut value) => {
            normalize_json(&mut value, None, side, mappings);
            ObservedBody::Json(value)
        }
        Err(_) => ObservedBody::Bytes(body.to_vec()),
    };
    HttpObservation {
        status,
        headers: semantic_headers,
        body,
    }
}

fn normalize_json(
    value: &mut Value,
    key: Option<&str>,
    side: ObservationSide,
    mappings: &[EndpointMapping],
) {
    match value {
        Value::Object(object) => {
            for (child_key, child) in object {
                normalize_json(child, Some(child_key), side, mappings);
            }
        }
        Value::Array(values) => {
            for child in values {
                normalize_json(child, key, side, mappings);
            }
        }
        Value::String(text) if key == Some("last_activity") => {
            if chrono::DateTime::parse_from_rfc3339(text).is_ok() {
                *text = "<date>".to_owned();
            }
        }
        Value::String(text) if key == Some("target") || key == Some("host") => {
            *text = normalize_structural_url_or_authority(text, side, mappings);
        }
        Value::String(text) if key == Some("x_forwarded_port") => {
            if let Some(mapping) = mappings.iter().find(|mapping| {
                mapping
                    .address(side)
                    .rsplit_once(':')
                    .is_some_and(|(_, port)| port == text)
            }) {
                *text = format!("{}-port", mapping.token());
            }
        }
        Value::String(text)
            if key == Some("x_forwarded_for") && text.parse::<std::net::IpAddr>().is_ok() =>
        {
            *text = "<client-address>".to_owned();
        }
        _ => {}
    }
}

fn normalize_structural_url_bytes(
    value: &[u8],
    side: ObservationSide,
    mappings: &[EndpointMapping],
) -> Vec<u8> {
    std::str::from_utf8(value).map_or_else(
        |_| value.to_vec(),
        |text| normalize_structural_url_or_authority(text, side, mappings).into_bytes(),
    )
}

fn normalize_structural_url_or_authority(
    input: &str,
    side: ObservationSide,
    mappings: &[EndpointMapping],
) -> String {
    for mapping in mappings {
        let address = mapping.address(side);
        if input == address {
            return mapping.token();
        }
        for prefix in ["http://", "https://", "ws://", "wss://"] {
            let needle = format!("{prefix}{address}");
            if input == needle || input.starts_with(&format!("{needle}/")) {
                return input.replacen(&needle, &format!("{prefix}{}", mapping.token()), 1);
            }
        }
    }
    input.to_owned()
}

#[derive(Clone, Debug, PartialEq)]
struct MetricSample {
    labels: BTreeMap<String, String>,
    value: f64,
}

#[derive(Debug)]
struct MetricFamily {
    help: String,
    kind: String,
    samples: BTreeMap<String, Vec<MetricSample>>,
}

type MetricKey = (String, BTreeMap<String, String>);
type CounterValues = BTreeMap<MetricKey, u64>;
type RawHttpObservation = (u16, Vec<(String, Vec<u8>)>, Vec<u8>);

pub(crate) fn compare_metric_expositions(left: &str, right: &str) -> Result<(), String> {
    let left = parse_metrics(left)?;
    let right = parse_metrics(right)?;
    for (name, kind, _) in METRIC_FAMILIES {
        let left_family = left.get(*name).ok_or_else(|| format!("missing {name}"))?;
        let right_family = right.get(*name).ok_or_else(|| format!("missing {name}"))?;
        if left_family.help != right_family.help || left_family.kind != right_family.kind {
            return Err(format!("metric metadata differs for {name}"));
        }
        if *kind == "summary" {
            compare_summary(name, left_family, right_family)?;
        } else if left_family.samples != right_family.samples {
            return Err(format!("counter samples differ for {name}"));
        }
    }
    Ok(())
}

fn compare_metric_deltas(
    left_before: &str,
    left_after: &str,
    right_before: &str,
    right_after: &str,
) -> Result<(), String> {
    let left_before = parse_metrics(left_before)?;
    let left_after = parse_metrics(left_after)?;
    let right_before = parse_metrics(right_before)?;
    let right_after = parse_metrics(right_after)?;
    for (name, kind, _) in METRIC_FAMILIES {
        let left = left_after
            .get(*name)
            .ok_or_else(|| format!("missing {name}"))?;
        let right = right_after
            .get(*name)
            .ok_or_else(|| format!("missing {name}"))?;
        if left.help != right.help || left.kind != right.kind {
            return Err(format!("metric metadata differs for {name}"));
        }
        if *kind == "summary" {
            compare_summary_structure(name, left, right)?;
            let count_name = format!("{name}_count");
            let left_delta = scalar_delta(left_before.get(*name).unwrap(), left, &count_name)?;
            let right_delta = scalar_delta(right_before.get(*name).unwrap(), right, &count_name)?;
            if left_delta != right_delta {
                return Err(format!(
                    "summary count delta differs for {name}: {left_delta} != {right_delta}"
                ));
            }
        } else {
            let left_delta = counter_delta(left_before.get(*name).unwrap(), left)?;
            let right_delta = counter_delta(right_before.get(*name).unwrap(), right)?;
            if left_delta != right_delta {
                return Err(format!(
                    "counter delta differs for {name}: {left_delta:?} != {right_delta:?}"
                ));
            }
        }
    }
    Ok(())
}

fn counter_delta(before: &MetricFamily, after: &MetricFamily) -> Result<CounterValues, String> {
    let before = counter_samples(before)?;
    let after = counter_samples(after)?;
    let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    keys.into_iter()
        .map(|key| {
            let old = before.get(&key).copied().unwrap_or(0);
            let new = after.get(&key).copied().unwrap_or(0);
            new.checked_sub(old)
                .map(|delta| (key, delta))
                .ok_or_else(|| "counter decreased".to_owned())
        })
        .filter(|entry| !matches!(entry, Ok((_, 0))))
        .collect()
}

fn counter_samples(family: &MetricFamily) -> Result<CounterValues, String> {
    let mut values = BTreeMap::new();
    for (sample_name, samples) in &family.samples {
        for sample in samples {
            if sample.value.fract() != 0.0 || sample.value < 0.0 {
                return Err(format!(
                    "counter {sample_name} is not a non-negative integer"
                ));
            }
            let key = (sample_name.clone(), sample.labels.clone());
            if values.insert(key, sample.value as u64).is_some() {
                return Err(format!("duplicate counter label set for {sample_name}"));
            }
        }
    }
    Ok(values)
}

fn scalar_delta(before: &MetricFamily, after: &MetricFamily, name: &str) -> Result<u64, String> {
    let value = |family: &MetricFamily| -> Result<u64, String> {
        let samples = family
            .samples
            .get(name)
            .ok_or_else(|| format!("missing {name}"))?;
        if samples.len() != 1 || samples[0].value.fract() != 0.0 || samples[0].value < 0.0 {
            return Err(format!("invalid scalar counter {name}"));
        }
        Ok(samples[0].value as u64)
    };
    value(after)?
        .checked_sub(value(before)?)
        .ok_or_else(|| format!("{name} decreased"))
}

fn parse_metrics(exposition: &str) -> Result<BTreeMap<String, MetricFamily>, String> {
    let expected: BTreeMap<_, _> = METRIC_FAMILIES
        .iter()
        .map(|(name, kind, help)| (*name, (*kind, *help)))
        .collect();
    let mut helps = BTreeMap::new();
    let mut types = BTreeMap::new();
    let mut samples: BTreeMap<String, BTreeMap<String, Vec<MetricSample>>> = BTreeMap::new();

    for line in exposition.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let (name, help) = rest
                .split_once(' ')
                .ok_or_else(|| format!("malformed HELP: {line}"))?;
            if is_chp_runtime_metric(name) {
                continue;
            }
            if !expected.contains_key(name)
                || helps.insert(name.to_owned(), help.to_owned()).is_some()
            {
                return Err(format!("unexpected or duplicate HELP family {name}"));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest
                .split_once(' ')
                .ok_or_else(|| format!("malformed TYPE: {line}"))?;
            if is_chp_runtime_metric(name) {
                continue;
            }
            if !expected.contains_key(name)
                || types.insert(name.to_owned(), kind.to_owned()).is_some()
            {
                return Err(format!("unexpected or duplicate TYPE family {name}"));
            }
            continue;
        }
        if line.starts_with('#') {
            return Err(format!("unexpected metric directive: {line}"));
        }
        let (sample, value) = line
            .split_once(' ')
            .ok_or_else(|| format!("malformed sample: {line}"))?;
        let value = value
            .parse::<f64>()
            .map_err(|_| format!("non-numeric sample: {line}"))?;
        if !value.is_finite() {
            return Err(format!("non-finite sample: {line}"));
        }
        let (sample_name, labels) = parse_sample_name(sample)?;
        if is_chp_runtime_metric(&sample_name) {
            continue;
        }
        let family_name = owning_family(&sample_name)
            .ok_or_else(|| format!("unexpected metric sample {sample_name}"))?;
        validate_labels(family_name, &sample_name, &labels)?;
        samples
            .entry(family_name.to_owned())
            .or_default()
            .entry(sample_name)
            .or_default()
            .push(MetricSample { labels, value });
    }

    let mut families = BTreeMap::new();
    for (name, (expected_kind, expected_help)) in expected {
        let help = helps
            .remove(name)
            .ok_or_else(|| format!("missing HELP {name}"))?;
        let kind = types
            .remove(name)
            .ok_or_else(|| format!("missing TYPE {name}"))?;
        if help != expected_help || kind != expected_kind {
            return Err(format!("wrong HELP or TYPE for {name}"));
        }
        let mut family_samples = samples.remove(name).unwrap_or_default();
        for values in family_samples.values_mut() {
            values.sort_by(|left, right| {
                left.labels
                    .cmp(&right.labels)
                    .then_with(|| left.value.total_cmp(&right.value))
            });
        }
        families.insert(
            name.to_owned(),
            MetricFamily {
                help,
                kind,
                samples: family_samples,
            },
        );
    }
    if !helps.is_empty() || !types.is_empty() || !samples.is_empty() {
        return Err("unexpected metric families remain".to_owned());
    }
    Ok(families)
}

fn is_chp_runtime_metric(name: &str) -> bool {
    CHP_RUNTIME_METRIC_FAMILIES.contains(&name)
        || ["_bucket", "_sum", "_count"].iter().any(|suffix| {
            name.strip_suffix(suffix)
                .is_some_and(|base| CHP_RUNTIME_METRIC_FAMILIES.contains(&base))
        })
}

fn parse_sample_name(sample: &str) -> Result<(String, BTreeMap<String, String>), String> {
    let Some((name, labels)) = sample.split_once('{') else {
        return Ok((sample.to_owned(), BTreeMap::new()));
    };
    let labels = labels
        .strip_suffix('}')
        .ok_or_else(|| format!("unclosed labels: {sample}"))?;
    let mut parsed = BTreeMap::new();
    for label in labels.split(',').filter(|label| !label.is_empty()) {
        let (key, value) = label
            .split_once('=')
            .ok_or_else(|| format!("malformed label: {sample}"))?;
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or_else(|| format!("unquoted label: {sample}"))?;
        if parsed.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(format!("duplicate label: {sample}"));
        }
    }
    Ok((name.to_owned(), parsed))
}

fn owning_family(sample: &str) -> Option<&'static str> {
    METRIC_FAMILIES.iter().find_map(|(name, kind, _)| {
        (sample == *name
            || (*kind == "summary"
                && (sample == format!("{name}_sum") || sample == format!("{name}_count"))))
        .then_some(*name)
    })
}

fn validate_labels(
    family: &str,
    sample: &str,
    labels: &BTreeMap<String, String>,
) -> Result<(), String> {
    if matches!(family, "requests_proxy" | "requests_api") {
        let status = labels
            .get("status")
            .filter(|_| labels.len() == 1)
            .ok_or_else(|| format!("{family} requires only status"))?;
        if !status.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("{family} status is not numeric"));
        }
    } else if matches!(family, "find_target_for_req" | "last_activity_updating") && sample == family
    {
        let quantile = labels
            .get("quantile")
            .filter(|_| labels.len() == 1)
            .ok_or_else(|| format!("{family} requires only quantile"))?;
        if !SUMMARY_QUANTILES.contains(&quantile.as_str()) {
            return Err(format!("unexpected {family} quantile {quantile}"));
        }
    } else if !labels.is_empty() {
        return Err(format!("unexpected labels on {sample}"));
    }
    Ok(())
}

fn compare_summary(name: &str, left: &MetricFamily, right: &MetricFamily) -> Result<(), String> {
    compare_summary_structure(name, left, right)?;
    let count_name = format!("{name}_count");
    if left.samples.get(&count_name) != right.samples.get(&count_name) {
        return Err(format!("summary count differs for {name}"));
    }
    Ok(())
}

fn compare_summary_structure(
    name: &str,
    left: &MetricFamily,
    right: &MetricFamily,
) -> Result<(), String> {
    let quantiles = |family: &MetricFamily| -> Result<Vec<String>, String> {
        let samples = family
            .samples
            .get(name)
            .ok_or_else(|| format!("missing {name} quantiles"))?;
        Ok(samples
            .iter()
            .map(|sample| sample.labels["quantile"].clone())
            .collect())
    };
    if quantiles(left)? != quantiles(right)? {
        return Err(format!("summary quantiles differ for {name}"));
    }
    let sum_name = format!("{name}_sum");
    for family in [left, right] {
        let sums = family
            .samples
            .get(&sum_name)
            .ok_or_else(|| format!("missing summary sum for {name}"))?;
        if sums.len() != 1 || sums[0].value < 0.0 {
            return Err(format!("invalid summary sum for {name}"));
        }
    }
    Ok(())
}

const STDERR_TAIL_CAPACITY: usize = 64 * 1024;
static ORACLE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
pub(crate) struct CapturedProcess {
    child: Child,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<JoinHandle<()>>,
}

#[allow(dead_code)]
impl CapturedProcess {
    pub(crate) fn spawn(mut command: Command) -> Self {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().expect("spawn differential process");
        let mut pipe = child.stderr.take().expect("differential stderr pipe");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&stderr);
        let stderr_thread = std::thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            while let Ok(count) = pipe.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                let redacted = redact_stderr(&chunk[..count]);
                let mut tail = captured
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                tail.extend_from_slice(&redacted);
                if tail.len() > STDERR_TAIL_CAPACITY {
                    let excess = tail.len() - STDERR_TAIL_CAPACITY;
                    tail.drain(..excess);
                }
            }
        });
        Self {
            child,
            stderr,
            stderr_thread: Some(stderr_thread),
        }
    }

    pub(crate) fn exited(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("poll differential process")
            .is_some()
    }

    pub(crate) fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(
            &self
                .stderr
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_owned()
    }

    fn join_stderr(&mut self) {
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CapturedProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.join_stderr();
    }
}

fn redact_stderr(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let workspace = env!("CARGO_MANIFEST_DIR");
    text.replace(workspace, "<workspace>")
        .bytes()
        .filter(|byte| *byte == b'\n' || *byte == b'\t' || !byte.is_ascii_control())
        .collect()
}

#[allow(dead_code)]
pub(crate) struct PortLease {
    listener: StdTcpListener,
}

#[allow(dead_code)]
impl PortLease {
    pub(crate) fn new() -> Self {
        Self {
            listener: StdTcpListener::bind(("127.0.0.1", 0)).expect("reserve differential port"),
        }
    }

    pub(crate) fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .expect("differential port address")
            .port()
    }
}

#[cfg(unix)]
pub(crate) struct LaunchLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl LaunchLock {
    pub(crate) fn acquire() -> Self {
        Self::acquire_at(std::path::Path::new("/tmp/pingora-chp-oracle-launch.lock"))
    }

    pub(crate) fn acquire_at(path: &std::path::Path) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .expect("open cross-process oracle launch lock");
        let result = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
        assert_eq!(result, 0, "acquire cross-process oracle launch lock");
        Self { file }
    }
}

#[cfg(unix)]
impl Drop for LaunchLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
    }
}

#[cfg(not(unix))]
struct LaunchLock;

#[cfg(not(unix))]
impl LaunchLock {
    fn acquire() -> Self {
        Self
    }
}

struct DifferentialEcho {
    port: u16,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl DifferentialEcho {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", 0))
            .await
            .expect("bind differential echo");
        let port = listener
            .local_addr()
            .expect("differential echo address")
            .port();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let router = Router::new().fallback(any(move |request: AxumRequest| async move {
                let (parts, body) = request.into_parts();
                let body = axum::body::to_bytes(body, 2 * 1024 * 1024)
                    .await
                    .expect("read differential echo body");
                if parts.uri.path().ends_with("/redirect") {
                    let forwarded = parts
                        .headers
                        .get("x-forwarded-for")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("127.0.0.1");
                    let echo_host = if forwarded == "127.0.0.1" {
                        format!("127.0.0.1:{port}")
                    } else {
                        format!("host.docker.internal:{port}")
                    };
                    return (
                        StatusCode::MOVED_PERMANENTLY,
                        [(http::header::LOCATION, format!("http://{echo_host}/next"))],
                        "",
                    )
                        .into_response();
                }
                (
                    [(CONTENT_TYPE, "application/json")],
                    serde_json::json!({
                        "method": parts.method.as_str(),
                        "url": parts.uri.to_string(),
                        "host": parts.headers.get(HOST).and_then(|value| value.to_str().ok()),
                        "x_forwarded_for": parts.headers.get("x-forwarded-for").and_then(|value| value.to_str().ok()),
                        "x_forwarded_port": parts.headers.get("x-forwarded-port").and_then(|value| value.to_str().ok()),
                        "x_forwarded_proto": parts.headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()),
                        "body": String::from_utf8_lossy(&body),
                    })
                    .to_string(),
                )
                    .into_response()
            }));
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve differential echo");
        });
        Self {
            port,
            stop: Some(stop),
            task,
        }
    }
}

impl Drop for DifferentialEcho {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

struct DockerOracle {
    process: CapturedProcess,
    name: String,
}

impl Drop for DockerOracle {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(unix)]
struct UnixPair {
    chp_public: std::path::PathBuf,
    chp_api: std::path::PathBuf,
    chp_metrics: std::path::PathBuf,
    rust_public: std::path::PathBuf,
    rust_api: std::path::PathBuf,
    rust_metrics: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

/// A live CHP 5.3.0 Node 20 container and Rust process sharing deterministic fixtures.
pub(crate) struct OraclePair {
    chp: DockerOracle,
    rust: CapturedProcess,
    chp_public: String,
    chp_public_port: u16,
    chp_api: String,
    chp_metrics: String,
    rust_public: String,
    rust_public_port: u16,
    rust_api: String,
    rust_metrics: String,
    mappings: Vec<EndpointMapping>,
    metric_baseline_chp: String,
    metric_baseline_rust: String,
    client: reqwest::Client,
    echo: DifferentialEcho,
    #[cfg(unix)]
    unix: Option<UnixPair>,
}

impl OraclePair {
    pub(crate) async fn start() -> Self {
        Self::start_with_args(&[]).await
    }

    pub(crate) async fn start_with_args(extra_args: &[String]) -> Self {
        let launch_lock = LaunchLock::acquire();
        let echo = DifferentialEcho::start().await;
        let rust_public_lease = PortLease::new();
        let rust_api_lease = PortLease::new();
        let rust_metrics_lease = PortLease::new();
        let rust_public_port = rust_public_lease.port();
        let rust_api_port = rust_api_lease.port();
        let rust_metrics_port = rust_metrics_lease.port();

        let mut chp_args = tcp_args(8000, 8001, 8002);
        chp_args.extend(extra_args.iter().map(|arg| dockerize_loopback(arg)));
        let (mut chp, [chp_public_port, chp_api_port, chp_metrics_port]) =
            spawn_docker_oracle(&chp_args, true, None).await;

        let mut rust_args = tcp_args(rust_public_port, rust_api_port, rust_metrics_port);
        rust_args.extend_from_slice(extra_args);
        let gate = tempfile::tempdir().expect("Rust oracle launch gate directory");
        let gate_path = gate.path().join("go");
        let mut rust_command = Command::new("sh");
        rust_command
            .arg("-c")
            .arg("while [ ! -e \"$1\" ]; do sleep 0.01; done; shift; exec \"$@\"")
            .arg("oracle-launch-gate")
            .arg(&gate_path)
            .arg(env!("CARGO_BIN_EXE_pingora-reverse-proxy"))
            .args(&rust_args);
        let mut rust = CapturedProcess::spawn(rust_command);
        assert!(!rust.exited(), "Rust child exited before launch handoff");
        drop((rust_public_lease, rust_api_lease, rust_metrics_lease));
        std::fs::File::create(&gate_path).expect("signal Rust bind handoff");

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("differential client");
        let chp_public = format!("http://127.0.0.1:{chp_public_port}");
        let chp_api = format!("http://127.0.0.1:{chp_api_port}");
        let chp_metrics = format!("http://127.0.0.1:{chp_metrics_port}");
        let rust_public = format!("http://127.0.0.1:{rust_public_port}");
        let rust_api = format!("http://127.0.0.1:{rust_api_port}");
        let rust_metrics = format!("http://127.0.0.1:{rust_metrics_port}");
        wait_tcp_ready(&client, &mut chp, &mut rust, &chp_api, &rust_api).await;
        drop(launch_lock);
        drop(gate);

        let mappings = vec![
            EndpointMapping::new(
                "public",
                &format!("127.0.0.1:{chp_public_port}"),
                &format!("127.0.0.1:{rust_public_port}"),
            ),
            EndpointMapping::new(
                "api",
                &format!("127.0.0.1:{chp_api_port}"),
                &format!("127.0.0.1:{rust_api_port}"),
            ),
            EndpointMapping::new(
                "metrics",
                &format!("127.0.0.1:{chp_metrics_port}"),
                &format!("127.0.0.1:{rust_metrics_port}"),
            ),
            EndpointMapping::new(
                "echo",
                &format!("host.docker.internal:{}", echo.port),
                &format!("127.0.0.1:{}", echo.port),
            ),
        ];
        let metric_baseline_chp = client
            .get(format!("{chp_metrics}/metrics"))
            .send()
            .await
            .expect("CHP baseline metrics")
            .text()
            .await
            .expect("CHP baseline metrics text");
        let metric_baseline_rust = client
            .get(format!("{rust_metrics}/metrics"))
            .send()
            .await
            .expect("Rust baseline metrics")
            .text()
            .await
            .expect("Rust baseline metrics text");
        Self {
            chp,
            rust,
            chp_public,
            chp_public_port,
            chp_api,
            chp_metrics,
            rust_public,
            rust_public_port,
            rust_api,
            rust_metrics,
            mappings,
            metric_baseline_chp,
            metric_baseline_rust,
            client,
            echo,
            #[cfg(unix)]
            unix: None,
        }
    }

    #[cfg(unix)]
    pub(crate) async fn start_unix() -> Self {
        let launch_lock = LaunchLock::acquire();
        let echo = DifferentialEcho::start().await;
        let directory = tempfile::tempdir().expect("differential Unix directory");
        let chp_public = directory.path().join("chp-public.sock");
        let chp_api = directory.path().join("chp-api.sock");
        let chp_metrics = directory.path().join("chp-metrics.sock");
        let rust_public = directory.path().join("rust-public.sock");
        let rust_api = directory.path().join("rust-api.sock");
        let rust_metrics = directory.path().join("rust-metrics.sock");
        let arguments =
            |public: &std::path::Path, api: &std::path::Path, metrics: &std::path::Path| {
                vec![
                    "--socket".to_owned(),
                    public.display().to_string(),
                    "--api-socket".to_owned(),
                    api.display().to_string(),
                    "--metrics-socket".to_owned(),
                    metrics.display().to_string(),
                    "--log-level".to_owned(),
                    "error".to_owned(),
                ]
            };
        let (mut chp, _) = spawn_docker_oracle(
            &arguments(&chp_public, &chp_api, &chp_metrics),
            false,
            Some(directory.path()),
        )
        .await;
        let mut rust_command = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"));
        rust_command.args(arguments(&rust_public, &rust_api, &rust_metrics));
        let mut rust = CapturedProcess::spawn(rust_command);
        wait_unix_ready(
            &mut chp,
            &mut rust,
            [&chp_public, &chp_api],
            [&rust_public, &rust_api],
        )
        .await;
        drop(launch_lock);
        let mappings = vec![
            EndpointMapping::new(
                "public",
                &chp_public.display().to_string(),
                &rust_public.display().to_string(),
            ),
            EndpointMapping::new(
                "api",
                &chp_api.display().to_string(),
                &rust_api.display().to_string(),
            ),
            EndpointMapping::new(
                "metrics",
                &chp_metrics.display().to_string(),
                &rust_metrics.display().to_string(),
            ),
            EndpointMapping::new(
                "echo",
                &format!("host.docker.internal:{}", echo.port),
                &format!("127.0.0.1:{}", echo.port),
            ),
        ];
        let metric_baseline_chp = String::from_utf8(
            docker_unix_raw_body(&chp.name, &chp_metrics, "GET", "/metrics", None).await,
        )
        .expect("CHP Unix baseline metrics");
        let metric_baseline_rust =
            String::from_utf8(unix_raw_body(&rust_metrics, "GET", "/metrics", None).await)
                .expect("Rust Unix baseline metrics");
        Self {
            chp,
            rust,
            chp_public: String::new(),
            chp_public_port: 0,
            chp_api: String::new(),
            chp_metrics: String::new(),
            rust_public: String::new(),
            rust_public_port: 0,
            rust_api: String::new(),
            rust_metrics: String::new(),
            mappings,
            metric_baseline_chp,
            metric_baseline_rust,
            client: reqwest::Client::new(),
            echo,
            unix: Some(UnixPair {
                chp_public,
                chp_api,
                chp_metrics,
                rust_public,
                rust_api,
                rust_metrics,
                _directory: directory,
            }),
        }
    }

    pub(crate) fn echo_target(&self) -> String {
        "http://oracle-echo.invalid".to_owned()
    }

    pub(crate) async fn post_route(&self, route: &str, body: Value) {
        let chp_body = self.body_for_side(body.clone(), ObservationSide::Chp);
        let rust_body = self.body_for_side(body, ObservationSide::Rust);
        #[cfg(unix)]
        if let Some(unix) = &self.unix {
            let chp = docker_unix_observation(
                &self.chp.name,
                &unix.chp_api,
                "POST",
                &format!("/api/routes{route}"),
                Some(&serde_json::to_vec(&chp_body).unwrap()),
                ObservationSide::Chp,
                &self.mappings,
            )
            .await;
            let rust = unix_observation(
                &unix.rust_api,
                "POST",
                &format!("/api/routes{route}"),
                Some(&serde_json::to_vec(&rust_body).unwrap()),
                ObservationSide::Rust,
                &self.mappings,
            )
            .await;
            assert_eq!(chp, rust, "CHP/Rust Unix route POST differs");
            return;
        }
        let chp = self
            .client
            .post(format!("{}/api/routes{route}", self.chp_api))
            .json(&chp_body)
            .send()
            .await
            .expect("CHP route POST");
        let rust = self
            .client
            .post(format!("{}/api/routes{route}", self.rust_api))
            .json(&rust_body)
            .send()
            .await
            .expect("Rust route POST");
        self.assert_same_response(chp, rust, "route POST").await;
    }

    pub(crate) async fn delete_route(&self, route: &str) {
        let chp = self
            .client
            .delete(format!("{}/api/routes{route}", self.chp_api))
            .send()
            .await
            .expect("CHP route DELETE");
        let rust = self
            .client
            .delete(format!("{}/api/routes{route}", self.rust_api))
            .send()
            .await
            .expect("Rust route DELETE");
        self.assert_same_response(chp, rust, "route DELETE").await;
    }

    pub(crate) async fn assert_same_http(&self, path: &str) {
        let chp = self
            .client
            .get(format!("{}{path}", self.chp_public))
            .send()
            .await
            .expect("CHP public request");
        let rust = self
            .client
            .get(format!("{}{path}", self.rust_public))
            .send()
            .await
            .expect("Rust public request");
        self.assert_same_response(chp, rust, path).await;
    }

    pub(crate) async fn assert_same_http_with_host(&self, path: &str, host: &str) {
        let chp = self
            .client
            .get(format!("{}{path}", self.chp_public))
            .header(HOST, host)
            .send()
            .await
            .expect("CHP public host request");
        let rust = self
            .client
            .get(format!("{}{path}", self.rust_public))
            .header(HOST, host)
            .send()
            .await
            .expect("Rust public host request");
        self.assert_same_response(chp, rust, path).await;
    }

    pub(crate) async fn assert_same_metrics(&self) {
        #[cfg(unix)]
        if let Some(unix) = &self.unix {
            let chp =
                docker_unix_raw_body(&self.chp.name, &unix.chp_metrics, "GET", "/metrics", None)
                    .await;
            let rust = unix_raw_body(&unix.rust_metrics, "GET", "/metrics", None).await;
            compare_metric_deltas(
                &self.metric_baseline_chp,
                &String::from_utf8_lossy(&chp),
                &self.metric_baseline_rust,
                &String::from_utf8_lossy(&rust),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "CHP/Rust Unix metric deltas differ: {error}; CHP stderr tail={}; Rust stderr tail={}",
                    self.chp.process.stderr_tail(),
                    self.rust.stderr_tail()
                )
            });
            return;
        }
        let chp = self
            .client
            .get(format!("{}/metrics", self.chp_metrics))
            .send()
            .await
            .expect("CHP metrics request")
            .text()
            .await
            .expect("CHP metrics text");
        let rust = self
            .client
            .get(format!("{}/metrics", self.rust_metrics))
            .send()
            .await
            .expect("Rust metrics request")
            .text()
            .await
            .expect("Rust metrics text");
        compare_metric_deltas(
            &self.metric_baseline_chp,
            &chp,
            &self.metric_baseline_rust,
            &rust,
        )
        .unwrap_or_else(|error| {
            panic!(
                "CHP/Rust metric deltas differ: {error}; CHP stderr tail={}; Rust stderr tail={}",
                self.chp.process.stderr_tail(),
                self.rust.stderr_tail()
            )
        });
    }

    pub(crate) async fn assert_same_websocket(&self, path: &str, payload: &[u8]) {
        let chp = websocket_observation(
            &format!(
                "ws://{}{}",
                self.chp_public.trim_start_matches("http://"),
                path
            ),
            payload,
        )
        .await;
        let rust = websocket_observation(
            &format!(
                "ws://{}{}",
                self.rust_public.trim_start_matches("http://"),
                path
            ),
            payload,
        )
        .await;
        assert_eq!(chp, rust, "CHP/Rust websocket observations differ");
    }

    #[cfg(unix)]
    pub(crate) async fn assert_same_unix_http(&self, path: &str) {
        let unix = self.unix.as_ref().expect("Unix OraclePair");
        let chp = docker_unix_observation(
            &self.chp.name,
            &unix.chp_public,
            "GET",
            path,
            None,
            ObservationSide::Chp,
            &self.mappings,
        )
        .await;
        let rust = unix_observation(
            &unix.rust_public,
            "GET",
            path,
            None,
            ObservationSide::Rust,
            &self.mappings,
        )
        .await;
        assert_eq!(chp, rust, "CHP/Rust Unix public response differs");
    }

    pub(crate) async fn assert_same_tls_health(&self, server_certificate: &std::path::Path) {
        let chp = tls_client(server_certificate, self.chp_public_port, None)
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.chp_public_port
            ))
            .send()
            .await
            .expect("CHP TLS health");
        let rust = tls_client(server_certificate, self.rust_public_port, None)
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.rust_public_port
            ))
            .send()
            .await
            .expect("Rust TLS health");
        self.assert_same_response(chp, rust, "TLS health").await;
    }

    pub(crate) async fn assert_same_mutual_tls_health(
        &self,
        server_certificate: &std::path::Path,
        client_certificate: &std::path::Path,
        client_key: &std::path::Path,
    ) {
        let mut identity = std::fs::read(client_certificate).expect("read client certificate");
        identity.extend_from_slice(&std::fs::read(client_key).expect("read client key"));
        let chp_client = tls_client(server_certificate, self.chp_public_port, Some(&identity));
        let rust_client = tls_client(server_certificate, self.rust_public_port, Some(&identity));
        let chp = chp_client
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.chp_public_port
            ))
            .send()
            .await
            .expect("CHP mutual TLS health");
        let rust = rust_client
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.rust_public_port
            ))
            .send()
            .await
            .expect("Rust mutual TLS health");
        self.assert_same_response(chp, rust, "mutual TLS health")
            .await;
        let chp_rejected = tls_client(server_certificate, self.chp_public_port, None)
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.chp_public_port
            ))
            .send()
            .await
            .is_err();
        let rust_rejected = tls_client(server_certificate, self.rust_public_port, None)
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.rust_public_port
            ))
            .send()
            .await
            .is_err();
        assert!(chp_rejected, "CHP accepted a client without mTLS identity");
        assert_eq!(chp_rejected, rust_rejected, "mTLS rejection differs");
    }

    pub(crate) async fn assert_same_route_tables(&self) {
        #[cfg(unix)]
        if let Some(unix) = &self.unix {
            let chp = docker_unix_observation(
                &self.chp.name,
                &unix.chp_api,
                "GET",
                "/api/routes",
                None,
                ObservationSide::Chp,
                &self.mappings,
            )
            .await;
            let rust = unix_observation(
                &unix.rust_api,
                "GET",
                "/api/routes",
                None,
                ObservationSide::Rust,
                &self.mappings,
            )
            .await;
            assert_eq!(chp, rust, "CHP/Rust Unix route tables differ");
            return;
        }
        let chp = self
            .client
            .get(format!("{}/api/routes", self.chp_api))
            .send()
            .await
            .expect("CHP route table");
        let rust = self
            .client
            .get(format!("{}/api/routes", self.rust_api))
            .send()
            .await
            .expect("Rust route table");
        self.assert_same_response(chp, rust, "route tables").await;
    }

    async fn assert_same_response(
        &self,
        chp: reqwest::Response,
        rust: reqwest::Response,
        scenario: &str,
    ) {
        let chp = observe(chp, ObservationSide::Chp, &self.mappings).await;
        let rust = observe(rust, ObservationSide::Rust, &self.mappings).await;
        assert_eq!(chp, rust, "CHP/Rust mismatch for {scenario}");
    }

    fn body_for_side(&self, mut body: Value, side: ObservationSide) -> Value {
        transform_targets(&mut body, side, self.echo.port);
        body
    }
}

fn tcp_args(public: u16, api: u16, metrics: u16) -> Vec<String> {
    vec![
        "--ip".into(),
        "0.0.0.0".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "0.0.0.0".into(),
        "--api-port".into(),
        api.to_string(),
        "--metrics-ip".into(),
        "0.0.0.0".into(),
        "--metrics-port".into(),
        metrics.to_string(),
        "--log-level".into(),
        "error".into(),
    ]
}

fn dockerize_loopback(arg: &str) -> String {
    arg.replace("127.0.0.1", "host.docker.internal")
}

fn transform_targets(value: &mut Value, side: ObservationSide, echo_port: u16) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "target" {
                    if let Value::String(target) = child {
                        let echo = match side {
                            ObservationSide::Chp => {
                                format!("host.docker.internal:{echo_port}")
                            }
                            ObservationSide::Rust => format!("127.0.0.1:{echo_port}"),
                        };
                        *target = target.replace("oracle-echo.invalid", &echo);
                        if side == ObservationSide::Chp {
                            *target = dockerize_loopback(target);
                        }
                    }
                } else {
                    transform_targets(child, side, echo_port);
                }
            }
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|child| transform_targets(child, side, echo_port)),
        _ => {}
    }
}

async fn spawn_docker_oracle(
    args: &[String],
    publish: bool,
    unix_mount: Option<&std::path::Path>,
) -> (DockerOracle, [u16; 3]) {
    let image = std::env::var("CHP_ORACLE_IMAGE")
        .expect("CHP_ORACLE_IMAGE must name the pinned Node 20 image");
    let name = format!(
        "chp-oracle-{}-{}",
        std::process::id(),
        ORACLE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let mut command = Command::new("docker");
    command.args([
        "run",
        "--rm",
        "--name",
        &name,
        "--add-host",
        "host.docker.internal:host-gateway",
    ]);
    if publish {
        for port in [8000, 8001, 8002] {
            command.args(["--publish", &format!("127.0.0.1::{port}")]);
        }
    }
    let workspace = env!("CARGO_MANIFEST_DIR");
    command.args(["--volume", &format!("{workspace}:{workspace}:ro")]);
    if let Some(path) = unix_mount {
        let path = path.display().to_string();
        command.args(["--volume", &format!("{path}:{path}")]);
    }
    command
        .arg(image)
        .args(["node", "/usr/local/bin/chp-oracle.mjs"])
        .args(args);
    let process = CapturedProcess::spawn(command);
    let mut oracle = DockerOracle { process, name };
    let ports = if publish {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            if oracle.process.exited() {
                panic!(
                    "Node 20 CHP container exited: {}",
                    oracle.process.stderr_tail()
                );
            }
            let found = [8000, 8001, 8002].map(|port| docker_mapped_port(&oracle.name, port));
            if let [Some(public), Some(api), Some(metrics)] = found {
                break [public, api, metrics];
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Docker port publication timed out: {}",
                oracle.process.stderr_tail()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    } else {
        [0, 0, 0]
    };
    (oracle, ports)
}

fn docker_mapped_port(name: &str, port: u16) -> Option<u16> {
    let output = Command::new("docker")
        .args(["port", name, &format!("{port}/tcp")])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .rsplit_once(':')?
                .1
                .parse()
                .ok()
        })
        .flatten()
}

async fn wait_tcp_ready(
    client: &reqwest::Client,
    chp: &mut DockerOracle,
    rust: &mut CapturedProcess,
    chp_api: &str,
    rust_api: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if chp.process.exited() {
            panic!("CHP readiness exit: {}", chp.process.stderr_tail());
        }
        if rust.exited() {
            panic!("Rust readiness exit: {}", rust.stderr_tail());
        }
        let mut ready = true;
        for url in [
            format!("{chp_api}/api/routes"),
            format!("{rust_api}/api/routes"),
        ] {
            ready &= client
                .get(url)
                .send()
                .await
                .is_ok_and(|response| response.status() == StatusCode::OK);
        }
        if ready {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pair readiness timed out; CHP={} Rust={}",
            chp.process.stderr_tail(),
            rust.stderr_tail()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
async fn wait_unix_ready(
    chp: &mut DockerOracle,
    rust: &mut CapturedProcess,
    chp_paths: [&std::path::Path; 2],
    rust_paths: [&std::path::Path; 2],
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if chp.process.exited() {
            panic!("CHP Unix exit: {}", chp.process.stderr_tail());
        }
        if rust.exited() {
            panic!("Rust Unix exit: {}", rust.stderr_tail());
        }
        let mut ready = chp_paths
            .into_iter()
            .all(|path| docker_unix_ready(&chp.name, path));
        for path in rust_paths {
            ready &= tokio::net::UnixStream::connect(path).await.is_ok();
        }
        if ready {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Unix readiness timed out; CHP={} Rust={}",
            chp.process.stderr_tail(),
            rust.stderr_tail()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
fn docker_unix_ready(container: &str, socket: &std::path::Path) -> bool {
    Command::new("docker")
        .args([
            "exec",
            container,
            "node",
            "-e",
            "const n=require('net');const s=n.createConnection({path:process.argv[1]},()=>{s.end();process.exit(0)});s.on('error',()=>process.exit(1));",
        ])
        .arg(socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn observe(
    response: reqwest::Response,
    side: ObservationSide,
    mappings: &[EndpointMapping],
) -> HttpObservation {
    let status = response.status().as_u16();
    let mut headers = Vec::new();
    for name in response.headers().keys() {
        for value in response.headers().get_all(name) {
            headers.push((name.as_str().to_owned(), value.as_bytes().to_vec()));
        }
    }
    let body = response.bytes().await.expect("read differential response");
    observation_from_parts(side, status, headers, &body, mappings)
}

#[cfg(unix)]
async fn unix_observation(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    side: ObservationSide,
    mappings: &[EndpointMapping],
) -> HttpObservation {
    let response = unix_raw_response(socket, method, path, body).await;
    let mut raw_headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut raw_headers);
    let offset = match parsed.parse(&response).expect("parse Unix HTTP response") {
        httparse::Status::Complete(offset) => offset,
        httparse::Status::Partial => panic!("partial Unix HTTP response"),
    };
    let status = parsed.code.expect("Unix HTTP status");
    let chunked = parsed.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(header.value).eq_ignore_ascii_case("chunked")
    });
    let headers = parsed
        .headers
        .iter()
        .map(|header| (header.name.to_owned(), header.value.to_vec()))
        .collect::<Vec<_>>();
    let body = if chunked {
        decode_chunked(&response[offset..])
    } else {
        response[offset..].to_vec()
    };
    observation_from_parts(side, status, headers, &body, mappings)
}

#[cfg(unix)]
async fn docker_unix_observation(
    container: &str,
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    side: ObservationSide,
    mappings: &[EndpointMapping],
) -> HttpObservation {
    let (status, headers, body) = docker_unix_http(container, socket, method, path, body).await;
    observation_from_parts(side, status, headers, &body, mappings)
}

#[cfg(unix)]
async fn docker_unix_raw_body(
    container: &str,
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Vec<u8> {
    docker_unix_http(container, socket, method, path, body)
        .await
        .2
}

#[cfg(unix)]
async fn docker_unix_http(
    container: &str,
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> RawHttpObservation {
    let container = container.to_owned();
    let socket = socket.to_owned();
    let method = method.to_owned();
    let path = path.to_owned();
    let body = body.unwrap_or_default().to_vec();
    tokio::task::spawn_blocking(move || {
        docker_unix_http_blocking(&container, &socket, &method, &path, &body)
    })
    .await
    .expect("join CHP Unix request")
}

#[cfg(unix)]
fn docker_unix_http_blocking(
    container: &str,
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: &[u8],
) -> RawHttpObservation {
    let hex: String = body.iter().map(|byte| format!("{byte:02x}")).collect();
    let output = Command::new("docker")
        .args([
            "exec", container, "node", "-e",
            "const h=require('http');const b=Buffer.from(process.argv[4],'hex');const q=h.request({socketPath:process.argv[1],method:process.argv[2],path:process.argv[3],headers:{host:'localhost','content-type':'application/json','content-length':b.length,connection:'close'}},r=>{const c=[];r.on('data',d=>c.push(d));r.on('end',()=>process.stdout.write(JSON.stringify({status:r.statusCode,headers:r.rawHeaders,body:Buffer.concat(c).toString('hex')})))});q.on('error',e=>{console.error(e.message);process.exit(1)});q.end(b);",
        ])
        .arg(socket)
        .arg(method)
        .arg(path)
        .arg(hex)
        .output()
        .expect("execute CHP Unix request inside container");
    assert!(
        output.status.success(),
        "CHP Unix request failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("CHP Unix response envelope");
    let status = envelope["status"].as_u64().expect("CHP Unix status") as u16;
    let raw_headers = envelope["headers"]
        .as_array()
        .expect("CHP Unix raw headers");
    let mut headers = Vec::new();
    for pair in raw_headers.chunks_exact(2) {
        headers.push((
            pair[0].as_str().expect("CHP Unix header name").to_owned(),
            pair[1]
                .as_str()
                .expect("CHP Unix header value")
                .as_bytes()
                .to_vec(),
        ));
    }
    let body = decode_hex(envelope["body"].as_str().expect("CHP Unix body hex"));
    (status, headers, body)
}

#[cfg(unix)]
fn decode_hex(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "even hexadecimal body");
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hexadecimal body"))
        .collect()
}

#[cfg(unix)]
async fn unix_raw_body(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Vec<u8> {
    let response = unix_raw_response(socket, method, path, body).await;
    let mut raw_headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut raw_headers);
    let offset = match parsed.parse(&response).expect("parse Unix HTTP response") {
        httparse::Status::Complete(offset) => offset,
        httparse::Status::Partial => panic!("partial Unix HTTP response"),
    };
    if parsed.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(header.value).eq_ignore_ascii_case("chunked")
    }) {
        decode_chunked(&response[offset..])
    } else {
        response[offset..].to_vec()
    }
}

#[cfg(unix)]
async fn unix_raw_response(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::UnixStream::connect(socket),
    )
    .await
    .expect("Unix connect timed out")
    .expect("Unix connect failed");
    let body = body.unwrap_or_default();
    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write Unix headers");
    stream.write_all(body).await.expect("write Unix body");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .expect("Unix response timed out")
        .expect("read Unix response");
    response
}

#[cfg(unix)]
fn decode_chunked(mut input: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|bytes| bytes == b"\r\n")
            .expect("chunk size terminator");
        let size = usize::from_str_radix(
            std::str::from_utf8(&input[..line_end])
                .expect("chunk size UTF-8")
                .split(';')
                .next()
                .expect("chunk size"),
            16,
        )
        .expect("hex chunk size");
        input = &input[line_end + 2..];
        if size == 0 {
            return decoded;
        }
        decoded.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
}

fn tls_client(root: &std::path::Path, port: u16, identity: Option<&[u8]>) -> reqwest::Client {
    let root = reqwest::Certificate::from_pem(&std::fs::read(root).expect("read TLS root"))
        .expect("parse TLS root");
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(root)
        .danger_accept_invalid_certs(true)
        .resolve(
            "openrusty.org",
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        );
    if let Some(identity) = identity {
        builder =
            builder.identity(reqwest::Identity::from_pem(identity).expect("parse mTLS identity"));
    }
    builder.build().expect("build TLS differential client")
}

async fn websocket_observation(url: &str, payload: &[u8]) -> (u16, String, Vec<u8>, bool) {
    let (mut socket, response) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .expect("websocket connect timed out")
    .expect("websocket connect failed");
    let greeting = socket
        .next()
        .await
        .expect("websocket greeting missing")
        .expect("websocket greeting failed")
        .into_text()
        .expect("websocket greeting text")
        .to_string();
    socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            payload.to_vec().into(),
        ))
        .await
        .expect("send websocket payload");
    let echoed = socket
        .next()
        .await
        .expect("websocket echo missing")
        .expect("websocket echo failed")
        .into_data()
        .to_vec();
    socket
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .expect("send websocket close");
    let closed = matches!(
        tokio::time::timeout(Duration::from_secs(3), socket.next()).await,
        Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))) | Ok(None)
    );
    (response.status().as_u16(), greeting, echoed, closed)
}
