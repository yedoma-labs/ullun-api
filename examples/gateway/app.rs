//! Gateway: application logic, shared by `main.rs` and the tests

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use ullun::observability::{AccessLog, Metrics, RequestIdLayer};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::security::constant_time_eq;

/// Headers that describe one connection, not the message (RFC 9110 section 7.6.1)
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone)]
pub struct RouteConfig {
    /// Requests below this path prefix are proxied, e.g. `/api/users`
    pub prefix: String,
    /// `http://host:port` of each upstream
    pub upstreams: Vec<String>,
    /// Remove the prefix before forwarding (`/api/users/7` -> `/7`)
    pub strip_prefix: bool,
}

pub struct Config {
    pub routes: Vec<RouteConfig>,
    pub admin_key: String,
    /// Time to get response headers from an upstream
    pub upstream_timeout: Duration,
    /// Extra attempts on other upstreams for idempotent requests
    pub retries: usize,
    /// Consecutive failures before an upstream is taken out of rotation
    pub failure_threshold: u32,
    /// How long it stays out before one request probes it again
    pub cooldown: Duration,
    /// Requests per window and client IP
    pub client_rate_limit: (u32, Duration),
    pub max_request_body: usize,
    pub log_requests: bool,
}

impl Config {
    pub fn from_env() -> std::result::Result<Self, String> {
        let dev = std::env::var("DEV").is_ok_and(|v| v == "1");
        let spec = match std::env::var("GATEWAY_ROUTES") {
            Ok(s) => s,
            Err(_) if dev => "/echo=http://127.0.0.1:9001".into(),
            Err(_) => {
                return Err("GATEWAY_ROUTES is required, e.g. /api=http://127.0.0.1:9000".into())
            }
        };
        let mut routes = Vec::new();
        for entry in spec.split(';').filter(|e| !e.trim().is_empty()) {
            let (prefix, upstreams) = entry.split_once('=').ok_or_else(|| {
                format!("bad route {entry:?}, expected prefix=upstream[,upstream]")
            })?;
            let upstreams: Vec<String> = upstreams
                .split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect();
            routes.push(RouteConfig {
                prefix: prefix.trim().to_string(),
                upstreams,
                strip_prefix: true,
            });
        }
        let admin_key = match std::env::var("GATEWAY_ADMIN_KEY") {
            Ok(k) if k.len() >= 16 => k,
            Ok(_) => return Err("GATEWAY_ADMIN_KEY must be at least 16 characters".into()),
            Err(_) if dev => "dev-admin-key-dev-admin-key".into(),
            Err(_) => return Err("GATEWAY_ADMIN_KEY is required (or DEV=1)".into()),
        };
        Ok(Self {
            routes,
            admin_key,
            upstream_timeout: Duration::from_secs(10),
            retries: 1,
            failure_threshold: 3,
            cooldown: Duration::from_secs(10),
            client_rate_limit: (600, Duration::from_secs(60)),
            max_request_body: 1024 * 1024,
            log_requests: true,
        })
    }
}

// ---- upstream pool ----

#[derive(Default)]
struct Health {
    consecutive_failures: u32,
    down_until: Option<Instant>,
    /// A probe request is on its way; others keep avoiding the upstream
    probing: bool,
}

struct Upstream {
    /// `scheme://authority`, validated at startup
    base: http::Uri,
    health: Mutex<Health>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Healthy,
    /// Out of rotation, cooldown not over
    Down,
    /// Cooldown over: this caller gets to probe it
    Probe,
}

impl Upstream {
    fn lock(&self) -> std::sync::MutexGuard<'_, Health> {
        self.health.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Decide whether this request may use the upstream (claims the probe slot)
    fn acquire(&self, now: Instant) -> Verdict {
        let mut h = self.lock();
        match h.down_until {
            None => Verdict::Healthy,
            Some(until) if now < until => Verdict::Down,
            Some(_) if h.probing => Verdict::Down,
            Some(_) => {
                h.probing = true;
                Verdict::Probe
            }
        }
    }

    fn succeeded(&self) {
        let mut h = self.lock();
        *h = Health::default();
    }

    fn failed(&self, threshold: u32, cooldown: Duration, now: Instant) {
        let mut h = self.lock();
        h.consecutive_failures += 1;
        h.probing = false;
        if h.consecutive_failures >= threshold {
            h.down_until = Some(now + cooldown);
        }
    }

    /// Give the probe slot back without judging (the request was not sent)
    fn release_probe(&self) {
        self.lock().probing = false;
    }
}

/// Holds a probe slot; releases it if the request is abandoned (client hung up,
/// early error) so that the upstream cannot stay "probing" forever
struct ProbeClaim<'a> {
    upstream: &'a Upstream,
    armed: bool,
}

impl ProbeClaim<'_> {
    /// The outcome was recorded; nothing to release
    fn done(mut self) {
        self.armed = false;
    }
}

impl Drop for ProbeClaim<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.upstream.release_probe();
        }
    }
}

struct RouteState {
    prefix: String,
    strip_prefix: bool,
    upstreams: Vec<Upstream>,
    next: AtomicUsize,
}

struct Gateway {
    routes: Vec<RouteState>,
    client: Client<HttpConnector, Full<Bytes>>,
    admin_key: String,
    timeout: Duration,
    retries: usize,
    failure_threshold: u32,
    cooldown: Duration,
}

fn parse_upstream(raw: &str) -> std::result::Result<http::Uri, String> {
    let uri: http::Uri = raw
        .parse()
        .map_err(|_| format!("upstream {raw:?} is not a valid URL"))?;
    if uri.scheme_str() != Some("http") || uri.authority().is_none() {
        return Err(format!("upstream {raw:?} must look like http://host:port (TLS is not supported in this example)"));
    }
    Ok(uri)
}

// ---- proxying ----

fn is_idempotent(method: &http::Method) -> bool {
    matches!(
        *method,
        http::Method::GET
            | http::Method::HEAD
            | http::Method::PUT
            | http::Method::DELETE
            | http::Method::OPTIONS
    )
}

/// Dot segments, backslashes and control characters (also percent-encoded) could
/// walk out of the route a request was authorised for.
fn path_is_suspicious(path: &str) -> bool {
    let decoded = urlencoding::decode(path)
        .map(|d| d.into_owned())
        .unwrap_or_else(|_| path.to_string());
    if decoded.contains('\\') || decoded.chars().any(char::is_control) || path.contains("//") {
        return true;
    }
    // decode twice: "%252e%252e" must not slip through either
    let twice = urlencoding::decode(&decoded)
        .map(|d| d.into_owned())
        .unwrap_or_else(|_| decoded.clone());
    twice.split('/').any(|seg| seg == ".." || seg == ".")
}

fn forward_headers(
    req: &Request,
    route_host: &str,
    remote: Option<std::net::IpAddr>,
) -> http::HeaderMap {
    // headers named by `Connection` are hop-by-hop too
    let extra: Vec<String> = req
        .headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|t| t.trim().to_ascii_lowercase()))
        .collect();

    let mut out = http::HeaderMap::new();
    for (name, value) in &req.headers {
        let n = name.as_str();
        // the client's claims about itself are not trusted: replaced below
        let skip = HOP_BY_HOP.contains(&n)
            || extra.iter().any(|e| e == n)
            || n == "host"
            || n == "content-length"
            || n == "x-forwarded-for"
            || n == "x-forwarded-host"
            || n == "x-forwarded-proto"
            || n == "forwarded"
            || n == "via";
        if !skip {
            out.append(name.clone(), value.clone());
        }
    }
    let set = |out: &mut http::HeaderMap, name: &'static str, value: String| {
        if let Ok(v) = http::HeaderValue::from_str(&value) {
            out.insert(http::HeaderName::from_static(name), v);
        }
    };
    set(&mut out, "host", route_host.to_string());
    if let Some(ip) = remote {
        set(&mut out, "x-forwarded-for", ip.to_string());
    }
    if let Some(host) = req
        .headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        set(&mut out, "x-forwarded-host", host.to_string());
    }
    set(&mut out, "x-forwarded-proto", "http".to_string());
    set(&mut out, "via", "1.1 ullun-gateway".to_string());
    if let Some(id) = req.request_id() {
        set(&mut out, "x-request-id", id.to_string());
    }
    out
}

async fn proxy(req: Request, route_idx: usize) -> Result<Response> {
    let gw = req.state::<Gateway>()?;
    let route = &gw.routes[route_idx];
    let full_path = req.uri.path();

    if req.headers.contains_key(http::header::UPGRADE) {
        return Err(Error::custom(
            501,
            "Upgrade requests (WebSocket) are not supported by this gateway",
        ));
    }
    if path_is_suspicious(full_path) {
        return Err(Error::bad_request("Invalid path"));
    }

    // path as the upstream sees it
    let rest = if route.strip_prefix {
        let stripped = &full_path[route.prefix.len().min(full_path.len())..];
        if stripped.is_empty() {
            "/"
        } else {
            stripped
        }
    } else {
        full_path
    };
    let path_and_query = match req.uri.query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest.to_string(),
    };

    let attempts = if is_idempotent(&req.method) {
        1 + gw.retries
    } else {
        1
    };
    let remote = req.remote_addr.map(|a| a.ip());
    let mut last_error: Option<Error> = None;
    let n = route.upstreams.len();
    let start = route.next.fetch_add(1, Ordering::Relaxed);
    let mut tried = 0;

    for offset in 0..n {
        if tried >= attempts {
            break;
        }
        let upstream = &route.upstreams[(start + offset) % n];
        let verdict = upstream.acquire(Instant::now());
        if verdict == Verdict::Down {
            continue;
        }
        tried += 1;
        let claim = ProbeClaim {
            upstream,
            armed: verdict == Verdict::Probe,
        };

        let authority = upstream
            .base
            .authority()
            .map(|a| a.as_str().to_string())
            .unwrap_or_default();
        let uri = format!("http://{authority}{path_and_query}");
        let mut builder = hyper::Request::builder()
            .method(req.method.clone())
            .uri(uri);
        if let Some(h) = builder.headers_mut() {
            *h = forward_headers(&req, &authority, remote);
        }
        let upstream_req = builder
            .body(Full::new(req.body.clone()))
            .map_err(|_| Error::bad_request("Invalid request"))?;

        let sent = tokio::time::timeout(gw.timeout, gw.client.request(upstream_req)).await;
        match sent {
            Err(_) => {
                upstream.failed(gw.failure_threshold, gw.cooldown, Instant::now());
                claim.done();
                last_error = Some(Error::custom(504, "Upstream timed out"));
            }
            Ok(Err(_)) => {
                upstream.failed(gw.failure_threshold, gw.cooldown, Instant::now());
                claim.done();
                last_error = Some(Error::custom(502, "Upstream unavailable"));
            }
            Ok(Ok(response)) => {
                if matches!(response.status().as_u16(), 502..=504) {
                    upstream.failed(gw.failure_threshold, gw.cooldown, Instant::now());
                } else {
                    upstream.succeeded();
                }
                claim.done();
                return Ok(relay(response));
            }
        }
    }

    // nothing worked (or every upstream is cooling down)
    Err(last_error.unwrap_or_else(|| Error::custom(503, "No healthy upstream")))
}

/// Upstream response -> client response, body streamed as it arrives
fn relay(response: hyper::Response<hyper::body::Incoming>) -> Response {
    let (parts, mut body) = response.into_parts();
    let (tx, mut out) = Response::stream();
    out.status = parts.status;

    let named_hop: Vec<String> = parts
        .headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|t| t.trim().to_ascii_lowercase()))
        .collect();
    for (name, value) in &parts.headers {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || named_hop.iter().any(|h| h == n) {
            continue;
        }
        out.headers.append(name.clone(), value.clone());
    }
    out.headers.insert(
        http::header::VIA,
        http::HeaderValue::from_static("1.1 ullun-gateway"),
    );

    tokio::spawn(async move {
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else { return };
            if let Ok(data) = frame.into_data() {
                // stops reading from the upstream as soon as the client is gone
                if tx.send(data).await.is_err() {
                    return;
                }
            }
        }
    });
    out
}

// ---- operator endpoints ----

async fn require_admin(req: Request, next: Next) -> Result<Response> {
    let key = req.state::<Gateway>()?.admin_key.clone();
    let given = req
        .headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(key.as_bytes(), given.as_bytes()) {
        return Err(Error::unauthorized("missing or invalid x-api-key"));
    }
    next.run(req).await
}

async fn status(req: Request) -> Result<Response> {
    let gw = req.state::<Gateway>()?;
    let now = Instant::now();
    let routes: Vec<_> = gw
        .routes
        .iter()
        .map(|r| {
            let upstreams: Vec<_> = r
                .upstreams
                .iter()
                .map(|u| {
                    let h = u.lock();
                    let state = match h.down_until {
                        None => "up",
                        Some(until) if now < until => "down",
                        Some(_) => "probing",
                    };
                    json!({
                        "url": u.base.to_string().trim_end_matches('/'),
                        "state": state,
                        "consecutive_failures": h.consecutive_failures,
                    })
                })
                .collect();
            json!({ "prefix": r.prefix, "upstreams": upstreams })
        })
        .collect();
    Ok(Response::json(json!({ "routes": routes })))
}

// ---- wiring ----

pub fn build(config: Config) -> App {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(config.upstream_timeout));
    connector.set_nodelay(true);
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .build(connector);

    let routes: Vec<RouteState> = config
        .routes
        .iter()
        .map(|r| {
            let upstreams = r
                .upstreams
                .iter()
                .map(|u| Upstream {
                    base: parse_upstream(u).unwrap_or_else(|e| panic!("{e}")),
                    health: Mutex::new(Health::default()),
                })
                .collect::<Vec<_>>();
            assert!(!upstreams.is_empty(), "route {} has no upstreams", r.prefix);
            RouteState {
                prefix: r.prefix.trim_end_matches('/').to_string(),
                strip_prefix: r.strip_prefix,
                upstreams,
                next: AtomicUsize::new(0),
            }
        })
        .collect();
    let prefixes: Vec<String> = routes.iter().map(|r| r.prefix.clone()).collect();

    let gateway = Gateway {
        routes,
        client,
        admin_key: config.admin_key,
        timeout: config.upstream_timeout,
        retries: config.retries,
        failure_threshold: config.failure_threshold.max(1),
        cooldown: config.cooldown,
    };

    let metrics = Metrics::new();
    let (limit, window) = config.client_rate_limit;
    let mut app = App::new()
        .state(gateway)
        .max_body_size(Some(config.max_request_body))
        .middleware(RequestIdLayer::new().ignore_incoming())
        .middleware(metrics.clone())
        .middleware(RateLimiter::new(limit, window));
    if config.log_requests {
        app = app.middleware(AccessLog::new().json());
    }

    app = app.group("/_gateway", |g| {
        g.middleware(require_admin)
            .get("/status", status)
            .get("/metrics", metrics.handler())
    });

    for (idx, prefix) in prefixes.iter().enumerate() {
        // the prefix itself (with and without slash) and everything below it,
        // for every method; the catch-all needs at least one character
        let slash = format!("{prefix}/");
        let below = format!("{prefix}/{{*rest}}");
        let handler = move |req: Request| proxy(req, idx);
        for path in [prefix.as_str(), slash.as_str(), below.as_str()] {
            app = app
                .get(path, handler)
                .post(path, handler)
                .put(path, handler)
                .patch(path, handler)
                .delete(path, handler);
        }
    }
    app
}
