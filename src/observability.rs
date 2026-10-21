//! Request ids, access logs and Prometheus metrics
//!
//! ```no_run
//! use ullun::observability::{AccessLog, Metrics, RequestIdLayer};
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! let metrics = Metrics::new();
//! App::new()
//!     .middleware(RequestIdLayer::new())      // first: the others read the id
//!     .middleware(AccessLog::new())           // one line per request
//!     .middleware(metrics.clone())            // counters and latency histograms
//!     .get("/metrics", metrics.handler())     // Prometheus text format
//!     .get("/users/{id}", |req: Request| async move {
//!         // requests of this route are logged and counted as `/users/{id}`,
//!         // not once per id
//!         Ok(Response::text(req.request_id().unwrap_or("-").to_string()))
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Metric labels use [`Request::matched_path`], the route *pattern*, so the
//! number of series is bounded by your routes, not by the ids or by whatever
//! paths scanners probe (all unmatched requests share `route="unmatched"`).
//! Do not expose `/metrics` publicly without authentication or a network
//! boundary.

use crate::error::Result;
use crate::files::iso8601;
use crate::handler::Handler;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::security::random_token;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ---- request ids ----

/// The id of the current request, stored in `Request::extensions`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(pub String);

impl Request {
    /// Id assigned by [`RequestIdLayer`], if installed
    pub fn request_id(&self) -> Option<&str> {
        self.extensions.get::<RequestId>().map(|r| r.0.as_str())
    }
}

fn valid_incoming_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Assigns every request an id (or keeps a sane one sent by the caller),
/// exposes it as [`Request::request_id`] and echoes it in a response header
pub struct RequestIdLayer {
    header: String,
    trust_incoming: bool,
}

impl RequestIdLayer {
    /// Header `x-request-id`, incoming ids accepted
    pub fn new() -> Self {
        Self {
            header: "x-request-id".into(),
            trust_incoming: true,
        }
    }

    pub fn header_name(mut self, name: impl Into<String>) -> Self {
        self.header = name.into().to_ascii_lowercase();
        self
    }

    /// Always generate a fresh id, even when the caller sent one (edge servers
    /// facing the internet)
    pub fn ignore_incoming(mut self) -> Self {
        self.trust_incoming = false;
        self
    }
}

impl Default for RequestIdLayer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Middleware for RequestIdLayer {
    async fn handle(&self, mut req: Request, next: Next) -> Result<Response> {
        let incoming = self
            .trust_incoming
            .then(|| {
                req.headers
                    .get(self.header.as_str())
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| valid_incoming_id(v))
                    .map(str::to_owned)
            })
            .flatten();
        let id = match incoming {
            Some(id) => id,
            None => random_token(8)?,
        };
        req.extensions.insert(RequestId(id.clone()));
        let res = match next.run(req).await {
            Ok(res) => res,
            Err(err) => Response::from_error(err),
        };
        Ok(res.with_header(self.header.clone(), id))
    }
}

// ---- access log ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Json,
}

/// One finished request, as handed to the log sink
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time: u64,
    pub remote: Option<String>,
    pub method: String,
    pub path: String,
    pub route: Option<String>,
    pub status: u16,
    pub bytes: Option<usize>,
    pub duration_ms: u128,
    pub request_id: Option<String>,
    /// Trace id of [`TraceLayer`](crate::trace::TraceLayer), when installed
    pub trace_id: Option<String>,
    pub user_agent: Option<String>,
}

/// Control characters would let a request forge log lines
fn clean(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

impl LogEntry {
    fn text(&self) -> String {
        let mut line = format!(
            "{} {} {} {} {} {} {}ms route={} id={} ua=\"{}\"",
            iso8601(self.time),
            self.remote.as_deref().unwrap_or("-"),
            self.method,
            self.path,
            self.status,
            self.bytes.map_or("-".to_string(), |b| format!("{b}B")),
            self.duration_ms,
            self.route.as_deref().unwrap_or("-"),
            self.request_id.as_deref().unwrap_or("-"),
            self.user_agent.as_deref().unwrap_or("-").replace('"', "'"),
        );
        if let Some(trace) = &self.trace_id {
            line.push_str(" trace=");
            line.push_str(trace);
        }
        line
    }

    fn json(&self) -> String {
        let mut entry = serde_json::json!({
            "time": iso8601(self.time),
            "remote": self.remote,
            "method": self.method,
            "path": self.path,
            "route": self.route,
            "status": self.status,
            "bytes": self.bytes,
            "duration_ms": self.duration_ms as u64,
            "request_id": self.request_id,
            "user_agent": self.user_agent,
        });
        if let Some(trace) = &self.trace_id {
            entry["trace_id"] = trace.clone().into();
        }
        entry.to_string()
    }
}

/// One log line per request, written when the response is ready
///
/// Lines go to stdout unless a [`sink`](AccessLog::sink) is set. Query strings
/// are left out by default because they often carry tokens.
pub struct AccessLog {
    format: Format,
    include_query: bool,
    sink: Arc<dyn Fn(&str) + Send + Sync>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl AccessLog {
    /// Plain text lines to stdout
    pub fn new() -> Self {
        Self {
            format: Format::Text,
            include_query: false,
            sink: Arc::new(|line| println!("{line}")),
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        }
    }

    /// One JSON object per line (for log shippers)
    pub fn json(mut self) -> Self {
        self.format = Format::Json;
        self
    }

    /// Log the query string too (careful with secrets)
    pub fn include_query(mut self) -> Self {
        self.include_query = true;
        self
    }

    /// Send lines somewhere else than stdout
    pub fn sink(mut self, sink: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.sink = Arc::new(sink);
        self
    }

    /// Replace the time source (tests)
    pub fn clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }
}

impl Default for AccessLog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Middleware for AccessLog {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let started = Instant::now();
        let method = req.method.to_string();
        let path = if self.include_query {
            req.uri
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or(req.uri.path())
        } else {
            req.uri.path()
        };
        let path = clean(path, 300);
        let remote = req.client_ip().map(|ip| ip.to_string());
        let route = req.matched_path.as_deref().map(str::to_owned);
        let user_agent = req
            .headers
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|v| clean(v, 200));
        let mut request_id = req.request_id().map(str::to_owned);
        let trace_id = req.trace().map(|t| t.trace_id.clone());

        let result = next.run(req).await;

        let (status, bytes) = match &result {
            Ok(res) => {
                if request_id.is_none() {
                    request_id = res
                        .headers
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                }
                (
                    res.status.as_u16(),
                    (!res.is_streaming()).then_some(res.body.len()),
                )
            }
            Err(err) => (err.status_code(), None),
        };
        let entry = LogEntry {
            time: (self.clock)(),
            remote,
            method,
            path,
            route,
            status,
            bytes,
            duration_ms: started.elapsed().as_millis(),
            request_id,
            trace_id,
            user_agent,
        };
        let line = match self.format {
            Format::Text => entry.text(),
            Format::Json => entry.json(),
        };
        (self.sink)(&line);
        result
    }
}

// ---- metrics ----

const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
/// Distinct (method, route, status) series; more are folded into `route="other"`
const MAX_SERIES: usize = 1000;

#[derive(Default)]
struct Histogram {
    buckets: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

#[derive(Default)]
struct Inner {
    requests: Mutex<HashMap<(String, String, u16), u64>>,
    durations: Mutex<HashMap<(String, String), Histogram>>,
    in_flight: AtomicI64,
    started: Option<Instant>,
}

/// Request counters, latency histograms and an in-flight gauge, rendered in the
/// Prometheus text format. Clone it: all clones share the numbers.
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

struct InFlight<'a>(&'a AtomicI64);

impl<'a> InFlight<'a> {
    fn new(counter: &'a AtomicI64) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // also runs when the request future is dropped (client went away)
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn known_method(m: &str) -> &str {
    match m {
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" => m,
        _ => "OTHER",
    }
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                started: Some(Instant::now()),
                ..Inner::default()
            }),
        }
    }

    /// Record one finished request
    pub fn observe(&self, method: &str, route: Option<&str>, status: u16, seconds: f64) {
        let method = known_method(method).to_string();
        let route = route.unwrap_or("unmatched").to_string();
        {
            let mut requests = lock(&self.inner.requests);
            let key = (method.clone(), route.clone(), status);
            let key = if requests.contains_key(&key) || requests.len() < MAX_SERIES {
                key
            } else {
                (method.clone(), "other".to_string(), status)
            };
            *requests.entry(key).or_insert(0) += 1;
        }
        let mut durations = lock(&self.inner.durations);
        let key = (method.clone(), route);
        let key = if durations.contains_key(&key) || durations.len() < MAX_SERIES {
            key
        } else {
            (method, "other".to_string())
        };
        let hist = durations.entry(key).or_default();
        hist.count += 1;
        hist.sum += seconds;
        for (i, le) in BUCKETS.iter().enumerate() {
            if seconds <= *le {
                hist.buckets[i] += 1;
            }
        }
    }

    /// Prometheus text exposition format
    pub fn render(&self) -> String {
        let mut out = String::new();

        out.push_str(
            "# HELP http_requests_total Total HTTP requests by method, route pattern and status.\n",
        );
        out.push_str("# TYPE http_requests_total counter\n");
        let requests = lock(&self.inner.requests);
        let mut rows: Vec<_> = requests.iter().collect();
        rows.sort();
        for ((method, route, status), n) in rows {
            out.push_str(&format!(
                "http_requests_total{{method=\"{}\",route=\"{}\",status=\"{}\"}} {}\n",
                method,
                escape_label(route),
                status,
                n
            ));
        }
        drop(requests);

        out.push_str(
            "# HELP http_request_duration_seconds Request latency by method and route pattern.\n",
        );
        out.push_str("# TYPE http_request_duration_seconds histogram\n");
        let durations = lock(&self.inner.durations);
        let mut rows: Vec<_> = durations.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        for ((method, route), h) in rows {
            let labels = format!("method=\"{}\",route=\"{}\"", method, escape_label(route));
            for (i, le) in BUCKETS.iter().enumerate() {
                out.push_str(&format!(
                    "http_request_duration_seconds_bucket{{{labels},le=\"{le}\"}} {}\n",
                    h.buckets[i]
                ));
            }
            out.push_str(&format!(
                "http_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}\n",
                h.count
            ));
            out.push_str(&format!(
                "http_request_duration_seconds_sum{{{labels}}} {}\n",
                h.sum
            ));
            out.push_str(&format!(
                "http_request_duration_seconds_count{{{labels}}} {}\n",
                h.count
            ));
        }
        drop(durations);

        out.push_str("# HELP http_requests_in_flight Requests being handled right now.\n");
        out.push_str("# TYPE http_requests_in_flight gauge\n");
        out.push_str(&format!(
            "http_requests_in_flight {}\n",
            self.inner.in_flight.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP process_uptime_seconds Seconds since the metrics were created.\n");
        out.push_str("# TYPE process_uptime_seconds gauge\n");
        out.push_str(&format!(
            "process_uptime_seconds {}\n",
            self.inner.started.map_or(0, |s| s.elapsed().as_secs())
        ));
        out
    }

    /// A handler serving [`render`](Metrics::render) with the Prometheus content type
    pub fn handler(&self) -> impl Handler {
        let metrics = self.clone();
        move |_req: Request| {
            let body = metrics.render();
            async move {
                Ok(Response::ok()
                    .with_header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                    .with_header("cache-control", "no-store")
                    .with_body(body))
            }
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Middleware for Metrics {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let started = Instant::now();
        let method = req.method.to_string();
        let route = req.matched_path.as_deref().map(str::to_owned);
        let guard = InFlight::new(&self.inner.in_flight);

        let result = next.run(req).await;

        let status = match &result {
            Ok(res) => res.status.as_u16(),
            Err(err) => err.status_code(),
        };
        drop(guard);
        self.observe(
            &method,
            route.as_deref(),
            status,
            started.elapsed().as_secs_f64(),
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_request_ids_are_validated() {
        assert!(valid_incoming_id("abc-123_X.y"));
        assert!(valid_incoming_id(&"a".repeat(64)));
        for bad in [
            "",
            "has space",
            "new\nline",
            "semi;colon",
            "<b>",
            &"a".repeat(65),
            "ümlaut",
        ] {
            assert!(!valid_incoming_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn log_lines_cannot_be_forged() {
        assert_eq!(clean("a\nb\r\tc", 100), "a?b??c");
        assert_eq!(clean(&"x".repeat(500), 10).len(), 10);
        let e = LogEntry {
            time: 784_111_777,
            remote: Some("10.0.0.1".into()),
            method: "GET".into(),
            path: "/a".into(),
            route: Some("/a".into()),
            status: 200,
            bytes: Some(12),
            duration_ms: 3,
            request_id: Some("rid".into()),
            trace_id: None,
            user_agent: Some("curl \"x\"".into()),
        };
        assert_eq!(
            e.text(),
            "1994-11-06T08:49:37Z 10.0.0.1 GET /a 200 12B 3ms route=/a id=rid ua=\"curl 'x'\""
        );
        let j: serde_json::Value = serde_json::from_str(&e.json()).unwrap();
        assert_eq!(
            (j["status"].as_u64(), j["bytes"].as_u64()),
            (Some(200), Some(12))
        );
        assert_eq!(j["user_agent"], "curl \"x\"");
    }

    #[test]
    fn metrics_render_prometheus_text() {
        let m = Metrics::new();
        m.observe("GET", Some("/users/{id}"), 200, 0.004);
        m.observe("GET", Some("/users/{id}"), 200, 0.3);
        m.observe("GET", Some("/users/{id}"), 404, 0.02);
        m.observe("PROPFIND", None, 404, 20.0);
        let text = m.render();

        assert!(text.contains(
            "http_requests_total{method=\"GET\",route=\"/users/{id}\",status=\"200\"} 2\n"
        ));
        assert!(text.contains(
            "http_requests_total{method=\"GET\",route=\"/users/{id}\",status=\"404\"} 1\n"
        ));
        // unknown methods and unmatched routes get fixed labels
        assert!(text.contains(
            "http_requests_total{method=\"OTHER\",route=\"unmatched\",status=\"404\"} 1\n"
        ));
        // histogram: cumulative buckets, sum and count
        let b = |le: &str| {
            format!("http_request_duration_seconds_bucket{{method=\"GET\",route=\"/users/{{id}}\",le=\"{le}\"}} ")
        };
        assert!(text.contains(&format!("{}1\n", b("0.005"))));
        assert!(text.contains(&format!("{}2\n", b("0.025"))));
        assert!(text.contains(&format!("{}3\n", b("0.5"))));
        assert!(text.contains(&format!("{}3\n", b("+Inf"))));
        assert!(text.contains(
            "http_request_duration_seconds_count{method=\"GET\",route=\"/users/{id}\"} 3\n"
        ));
        // slower than every bucket: only +Inf counts it
        assert!(text.contains("route=\"unmatched\",le=\"10\"} 0\n"));
        assert!(text.contains("route=\"unmatched\",le=\"+Inf\"} 1\n"));
        assert!(text.contains("# TYPE http_requests_total counter\n"));
        assert!(text.contains("http_requests_in_flight 0\n"));
    }

    #[test]
    fn label_values_are_escaped_and_series_are_capped() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");

        let m = Metrics::new();
        for i in 0..(MAX_SERIES + 50) {
            m.observe("GET", Some(&format!("/r{i}")), 200, 0.001);
        }
        let text = m.render();
        let series = text
            .lines()
            .filter(|l| l.starts_with("http_requests_total{"))
            .count();
        assert!(series <= MAX_SERIES + 1, "{series}");
        assert!(text.contains("route=\"other\""));
        // existing series keep counting after the cap is reached
        m.observe("GET", Some("/r0"), 200, 0.001);
        assert!(m.render().contains("route=\"/r0\",status=\"200\"} 2\n"));
    }
}
