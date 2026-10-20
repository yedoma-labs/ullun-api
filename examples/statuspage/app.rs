//! Status page: application logic, shared by `main.rs` and the tests

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, Semaphore};
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;
use ullun::security::{constant_time_eq, resolve_public};
use ullun::sse::{sse, Event};
use ullun::validate::Validator;

const HISTORY: usize = 100;
const MAX_MONITORS: usize = 50;
const MAX_PARALLEL_CHECKS: usize = 16;

pub struct Config {
    pub admin_key: String,
    /// Allow targets resolving to loopback, private or link-local addresses
    pub allow_private_targets: bool,
    /// Scheduler resolution; `interval_secs` is multiplied by `interval_unit`
    pub tick: Duration,
    pub interval_unit: Duration,
    /// Consecutive failures before a monitor counts as down
    pub failure_threshold: usize,
    pub check_timeout: Duration,
}

impl Config {
    pub fn from_env() -> std::result::Result<Self, String> {
        let dev = std::env::var("DEV").is_ok_and(|v| v == "1");
        let admin_key = match std::env::var("ADMIN_KEY") {
            Ok(k) if k.len() >= 16 => k,
            Ok(_) => return Err("ADMIN_KEY must be at least 16 characters".into()),
            Err(_) if dev => "dev-admin-key-dev-admin-key".to_string(),
            Err(_) => return Err("ADMIN_KEY is required (or DEV=1 for a demo key)".into()),
        };
        Ok(Self {
            admin_key,
            allow_private_targets: dev
                || std::env::var("ALLOW_PRIVATE_TARGETS").is_ok_and(|v| v == "1"),
            tick: Duration::from_secs(1),
            interval_unit: Duration::from_secs(1),
            failure_threshold: 2,
            check_timeout: Duration::from_secs(5),
        })
    }
}

// ---- model ----

#[derive(Clone, Debug)]
enum Target {
    Http {
        host: String,
        port: u16,
        path: String,
    },
    Tcp {
        host: String,
        port: u16,
    },
}

impl Target {
    fn parse(raw: &str) -> std::result::Result<Target, String> {
        let uri: http::Uri = raw.parse().map_err(|_| "is not a valid URL".to_string())?;
        let host = uri
            .host()
            .filter(|h| !h.is_empty())
            .ok_or("needs a host")?
            .to_string();
        match uri.scheme_str() {
            Some("http") => Ok(Target::Http {
                port: uri.port_u16().unwrap_or(80),
                path: uri
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .filter(|p| p.starts_with('/'))
                    .unwrap_or_else(|| "/".into()),
                host,
            }),
            Some("tcp") => Ok(Target::Tcp {
                port: uri.port_u16().ok_or("tcp targets need a port, e.g. tcp://db.internal:5432")?,
                host,
            }),
            Some("https") => Err("https needs TLS, which this example does not include; use tcp://host:443 for reachability".into()),
            _ => Err("must start with http:// or tcp://".into()),
        }
    }

    fn host_port(&self) -> (&str, u16) {
        match self {
            Target::Http { host, port, .. } | Target::Tcp { host, port } => (host, *port),
        }
    }
}

#[derive(Clone, Serialize)]
struct CheckResult {
    at: u64,
    ok: bool,
    latency_ms: u64,
    detail: String,
}

struct Monitor {
    id: u32,
    name: String,
    target_text: String,
    target: Target,
    interval_secs: u64,
    history: VecDeque<CheckResult>,
    next_due: Instant,
    running: bool,
}

#[derive(Default)]
struct Data {
    next_id: u32,
    monitors: BTreeMap<u32, Monitor>,
}

struct Store {
    data: Mutex<Data>,
    templates: Templates,
    events: broadcast::Sender<String>,
    permits: Semaphore,
    admin_key: String,
    allow_private: bool,
    interval_unit: Duration,
    tick: Duration,
    failure_threshold: usize,
    timeout: Duration,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status_of(&self, m: &Monitor) -> &'static str {
        let Some(last) = m.history.back() else {
            return "unknown";
        };
        if last.ok {
            return "up";
        }
        let failures = m.history.iter().rev().take_while(|c| !c.ok).count();
        if failures >= self.failure_threshold {
            "down"
        } else {
            "degraded"
        }
    }

    fn view(&self, m: &Monitor) -> Value {
        let total = m.history.len();
        let up = m.history.iter().filter(|c| c.ok).count();
        let last = m.history.back();
        json!({
            "id": m.id,
            "name": m.name,
            "status": self.status_of(m),
            "uptime_percent": (up * 1000).checked_div(total).map(|t| json!(t as f64 / 10.0)),
            "checks": total,
            "last_check": last.map(|c| c.at),
            "latency_ms": last.map(|c| c.latency_ms),
            "detail": last.map(|c| c.detail.clone()),
        })
    }

    fn snapshot(&self) -> Value {
        let data = self.lock();
        let monitors: Vec<Value> = data.monitors.values().map(|m| self.view(m)).collect();
        let overall = if monitors.iter().any(|m| m["status"] == "down") {
            "major_outage"
        } else if monitors.iter().any(|m| m["status"] == "degraded") {
            "degraded"
        } else if monitors.iter().all(|m| m["status"] == "unknown") {
            "unknown"
        } else {
            "operational"
        };
        json!({ "overall": overall, "monitors": monitors, "generated_at": now_secs() })
    }

    fn publish(&self) {
        // no subscribers is fine
        let _ = self.events.send(self.snapshot().to_string());
    }

    /// Run one check for `id` and record it; `None` if the monitor is gone
    async fn run_check(&self, id: u32) -> Option<CheckResult> {
        let target = {
            let mut data = self.lock();
            let m = data.monitors.get_mut(&id)?;
            m.running = true;
            m.target.clone()
        };
        let _permit = self.permits.acquire().await.ok()?;
        let started = Instant::now();
        let outcome = check_target(&target, self.timeout, self.allow_private).await;
        let result = CheckResult {
            at: now_secs(),
            ok: outcome.is_ok(),
            latency_ms: started.elapsed().as_millis() as u64,
            detail: match outcome {
                Ok(detail) | Err(detail) => detail,
            },
        };
        {
            let mut data = self.lock();
            let m = data.monitors.get_mut(&id)?; // deleted while checking
            m.history.push_back(result.clone());
            while m.history.len() > HISTORY {
                m.history.pop_front();
            }
            m.running = false;
            m.next_due = Instant::now() + self.interval_unit * m.interval_secs as u32;
        }
        self.publish();
        Some(result)
    }
}

// ---- checking ----

async fn check_target(
    target: &Target,
    timeout: Duration,
    allow_private: bool,
) -> std::result::Result<String, String> {
    let (host, port) = target.host_port();
    let addr = resolve_public(host, port, allow_private)
        .await
        .map_err(|e| e.to_string())?;
    let run = async {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|_| "connection failed".to_string())?;
        match target {
            Target::Tcp { .. } => Ok("connected".to_string()),
            Target::Http { host, port, path } => http_status(stream, host, *port, path).await,
        }
    };
    tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| "timeout".to_string())?
}

async fn http_status(
    stream: TcpStream,
    host: &str,
    port: u16,
    path: &str,
) -> std::result::Result<String, String> {
    use http_body_util::Empty;
    use hyper_util::rt::TokioIo;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| "not an HTTP server".to_string())?;
    tokio::spawn(conn);
    let host_header = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let req = hyper::Request::builder()
        .uri(path)
        .header("host", host_header)
        .header("user-agent", "ullun-statuspage")
        .body(Empty::<bytes::Bytes>::new())
        .map_err(|_| "bad request target".to_string())?;
    let res = sender
        .send_request(req)
        .await
        .map_err(|_| "no valid HTTP response".to_string())?;
    let status = res.status();
    if status.as_u16() < 400 {
        Ok(format!("HTTP {}", status.as_u16()))
    } else {
        Err(format!("HTTP {}", status.as_u16()))
    }
}

// ---- background scheduler ----

async fn scheduler(ctx: Context) {
    let Ok(store) = ctx.state::<Store>() else {
        return;
    };
    let mut running = tokio::task::JoinSet::new();
    while ctx.sleep(store.tick).await {
        let due: Vec<u32> = {
            let mut data = store.lock();
            let now = Instant::now();
            data.monitors
                .values_mut()
                .filter(|m| !m.running && m.next_due <= now)
                .map(|m| {
                    m.running = true; // claimed; run_check keeps it set
                    m.id
                })
                .collect()
        };
        for id in due {
            let ctx = ctx.clone();
            running.spawn(async move {
                if let Ok(store) = ctx.state::<Store>() {
                    store.run_check(id).await;
                }
            });
        }
        while running.try_join_next().is_some() {}
    }
    // shutdown: let started checks finish (they are bounded by the check timeout)
    while running.join_next().await.is_some() {}
}

// ---- handlers ----

fn store(req: &Request) -> Result<&Store> {
    req.state::<Store>()
}

async fn require_admin(req: Request, next: Next) -> Result<Response> {
    let key = store(&req)?.admin_key.as_bytes();
    let given = req
        .headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(key, given.as_bytes()) {
        return Err(Error::unauthorized("missing or invalid x-api-key"));
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct NewMonitor {
    name: String,
    target: String,
    interval_secs: Option<u64>,
}

async fn create_monitor(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let body: NewMonitor = req.json()?;
    let interval = body.interval_secs.unwrap_or(30);

    let mut v = Validator::new();
    v.len_between("name", body.name.trim(), 1, 60);
    v.range("interval_secs", interval, 1, 3600);
    let target = match Target::parse(body.target.trim()) {
        Ok(t) => Some(t),
        Err(e) => {
            v.error("target", e.to_string());
            None
        }
    };
    v.finish()?;
    let target = target.expect("validated");

    // refuse private targets at creation too, so the admin gets the error immediately
    let (host, port) = target.host_port();
    if let Err(e) = resolve_public(host, port, st.allow_private).await {
        let mut v = Validator::new();
        v.error("target", e.to_string());
        v.finish()?;
    }

    let mut data = st.lock();
    if data.monitors.len() >= MAX_MONITORS {
        return Err(Error::custom(
            409,
            format!("at most {MAX_MONITORS} monitors"),
        ));
    }
    data.next_id += 1;
    let id = data.next_id;
    data.monitors.insert(
        id,
        Monitor {
            id,
            name: body.name.trim().to_string(),
            target_text: body.target.trim().to_string(),
            target,
            interval_secs: interval,
            history: VecDeque::new(),
            next_due: Instant::now(), // first check on the next tick
            running: false,
        },
    );
    let view = st.view(&data.monitors[&id]);
    drop(data);
    st.publish();
    Ok(Response::json(view)
        .with_header("location", format!("/api/monitors/{id}"))
        .with_status(http::StatusCode::CREATED))
}

async fn list_monitors(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let data = st.lock();
    let items: Vec<Value> = data
        .monitors
        .values()
        .map(|m| {
            let mut v = st.view(m);
            v["target"] = json!(m.target_text);
            v["interval_secs"] = json!(m.interval_secs);
            v
        })
        .collect();
    Ok(Response::json(json!({ "items": items })))
}

async fn delete_monitor(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let id: u32 = req.params.parse("id")?;
    let removed = st.lock().monitors.remove(&id).is_some();
    if !removed {
        return Err(Error::not_found("no such monitor"));
    }
    st.publish();
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

/// Check right now instead of waiting for the schedule
async fn check_now(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let id: u32 = req.params.parse("id")?;
    let result = st
        .run_check(id)
        .await
        .ok_or_else(|| Error::not_found("no such monitor"))?;
    Ok(Response::json(result))
}

async fn history(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let id: u32 = req.params.parse("id")?;
    let data = st.lock();
    let m = data
        .monitors
        .get(&id)
        .ok_or_else(|| Error::not_found("no such monitor"))?;
    Ok(Response::json(json!({ "id": id, "checks": m.history })))
}

async fn status_json(req: Request) -> Result<Response> {
    Ok(Response::json(store(&req)?.snapshot()).with_header("cache-control", "no-cache"))
}

/// Live updates: the current state first, then every change
async fn events(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let mut updates = st.events.subscribe();
    let first = st.snapshot().to_string();
    let shutdown = req.shutdown_signal();
    let (tx, res) = sse();

    tokio::spawn(async move {
        if tx.send(Event::data(first).event("status")).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                _ = shutdown.wait() => {
                    let _ = tx.send(Event::comment("server shutting down").retry(5000)).await;
                    return;
                }
                _ = tx.closed() => return,
                update = updates.recv() => match update {
                    Ok(json) => {
                        if tx.send(Event::data(json).event("status")).await.is_err() {
                            return;
                        }
                    }
                    // fell behind: the next update carries the full state anyway
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                // keep proxies from closing an idle connection
                _ = tokio::time::sleep(Duration::from_secs(20)) => {
                    if tx.send(Event::comment("keep-alive")).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    Ok(res)
}

fn page_headers(res: Response) -> Response {
    res.with_header(
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'",
    )
    .with_header("x-content-type-options", "nosniff")
    .with_header("referrer-policy", "no-referrer")
}

async fn index(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let html = st.templates.render("index.html", &st.snapshot())?;
    Ok(page_headers(Response::html(html)))
}

const CLIENT_JS: &str = r#"
const list = document.getElementById("monitors");
const banner = document.getElementById("overall");
const labels = { operational: "All systems operational", degraded: "Partial degradation", major_outage: "Major outage", unknown: "No data yet" };
function render(s) {
  banner.textContent = labels[s.overall] || s.overall;
  banner.className = "banner " + s.overall;
  list.replaceChildren(...s.monitors.map(m => {
    const li = document.createElement("li");
    const dot = document.createElement("span"); dot.className = "dot " + m.status;
    const name = document.createElement("strong"); name.textContent = m.name;
    const meta = document.createElement("span"); meta.className = "meta";
    meta.textContent = m.status + (m.uptime_percent == null ? "" : " · " + m.uptime_percent + "% uptime · " + m.latency_ms + " ms");
    li.append(dot, name, meta);
    return li;
  }));
}
const es = new EventSource("/events");
es.addEventListener("status", e => render(JSON.parse(e.data)));
"#;

async fn client_js(_req: Request) -> Result<Response> {
    Ok(Response::ok()
        .with_header("content-type", "application/javascript; charset=utf-8")
        .with_header("cache-control", "public, max-age=300")
        .with_body(CLIENT_JS))
}

fn templates() -> Templates {
    let mut t = Templates::new();
    t.add(
        "index.html",
        r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Status</title>
<style>
 body{font:16px/1.5 system-ui;max-width:40rem;margin:2rem auto;padding:0 1rem}
 .banner{padding:1rem;border-radius:.5rem;color:#fff;background:#666}
 .banner.operational{background:#1a7f37}.banner.degraded{background:#9a6700}.banner.major_outage{background:#cf222e}
 ul{list-style:none;padding:0}li{display:flex;gap:.6rem;align-items:center;padding:.6rem 0;border-bottom:1px solid #ddd}
 .meta{margin-left:auto;color:#666;font-size:.9rem}
 .dot{width:.8rem;height:.8rem;border-radius:50%;background:#999}.dot.up{background:#1a7f37}.dot.degraded{background:#9a6700}.dot.down{background:#cf222e}
</style>
<body>
<h1>Service status</h1>
<div id="overall" class="banner {{overall}}">{{overall}}</div>
<ul id="monitors">
{{#each monitors}}<li><span class="dot {{status}}"></span><strong>{{name}}</strong><span class="meta">{{status}}{{#if uptime_percent}} · {{uptime_percent}}% uptime · {{latency_ms}} ms{{/if}}</span></li>
{{/each}}</ul>
<script src="/app.js"></script>
</body></html>"#,
    )
    .expect("template parses");
    t
}

pub fn build(config: Config) -> App {
    let (events_tx, _) = broadcast::channel(64);
    let store = Store {
        data: Mutex::new(Data::default()),
        templates: templates(),
        events: events_tx,
        permits: Semaphore::new(MAX_PARALLEL_CHECKS),
        admin_key: config.admin_key,
        allow_private: config.allow_private_targets,
        interval_unit: config.interval_unit,
        tick: config.tick,
        failure_threshold: config.failure_threshold.max(1),
        timeout: config.check_timeout,
    };

    let admin = |s: &str| {
        Operation::new(s)
            .tag("admin")
            .secured_by_api_key("x-api-key")
            .error_response(401, "Missing or invalid admin key")
    };

    App::new()
        .state(store)
        .background(scheduler)
        .max_body_size(Some(8 * 1024))
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(json!({ "status": "ok" })))
        })
        .get("/", index)
        .get("/app.js", client_js)
        .get("/events", events)
        .doc(
            "GET",
            "/events",
            Operation::new("Live status updates (Server-Sent Events, event \"status\")")
                .tag("public")
                .response(200, "text/event-stream"),
        )
        .get("/api/status", status_json)
        .doc(
            "GET",
            "/api/status",
            Operation::new("Overall status and per-monitor summary").tag("public"),
        )
        .group("/api/monitors", |g| {
            g.middleware(require_admin)
                .post("", create_monitor)
                .doc(
                    "POST",
                    "",
                    admin("Add a monitor")
                        .request_json(schema::object(
                            [
                                ("name", schema::string()),
                                ("target", schema::string()),
                                ("interval_secs", schema::integer()),
                            ],
                            ["name", "target"],
                        ))
                        .error_response(
                            422,
                            "Invalid name, target or interval (also: target not public)",
                        ),
                )
                .get("", list_monitors)
                .doc("GET", "", admin("List monitors with their targets"))
                .delete("/{id}", delete_monitor)
                .doc(
                    "DELETE",
                    "/{id}",
                    admin("Remove a monitor").error_response(404, "No such monitor"),
                )
                .post("/{id}/check", check_now)
                .doc(
                    "POST",
                    "/{id}/check",
                    admin("Check now").error_response(404, "No such monitor"),
                )
                .get("/{id}/history", history)
                .doc(
                    "GET",
                    "/{id}/history",
                    admin("Last 100 results").error_response(404, "No such monitor"),
                )
        })
        .openapi(OpenApiInfo::new("Status page", "1.0.0").swagger_ui("/docs"))
}
