//! Webhooks: application logic, shared by `main.rs` and the tests

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use ullun::jobs::{EnqueueOptions, Job, JobError, JobId, JobQueue, QueueConfig};
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;
use ullun::security::{
    constant_time_eq, random_alphanumeric, random_token, resolve_public, ResolveError,
};
use ullun::validate::Validator;

type HmacSha256 = Hmac<Sha256>;

const MAX_ENDPOINTS: usize = 100;
const LOG_PER_ENDPOINT: usize = 50;
const MAX_IDEMPOTENCY_KEYS: usize = 10_000;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

pub struct Config {
    pub api_key: String,
    pub allow_private_targets: bool,
    pub workers: usize,
    pub delivery_timeout: Duration,
    /// Dead deliveries in a row before an endpoint is switched off
    pub disable_after: u32,
    pub max_payload_bytes: usize,
    pub data_dir: Option<PathBuf>,
    /// Retry policy of the delivery queue (its file is set from `data_dir`)
    pub queue: QueueConfig,
    pub max_attempts: u32,
    /// Seconds since the Unix epoch
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Config {
    pub fn from_env() -> std::result::Result<Self, String> {
        let dev = std::env::var("DEV").is_ok_and(|v| v == "1");
        let api_key = match std::env::var("WEBHOOKS_API_KEY") {
            Ok(k) if k.len() >= 16 => k,
            Ok(_) => return Err("WEBHOOKS_API_KEY must be at least 16 characters".into()),
            Err(_) if dev => "dev-webhooks-key-dev-key".into(),
            Err(_) => return Err("WEBHOOKS_API_KEY is required (or DEV=1)".into()),
        };
        Ok(Self {
            api_key,
            allow_private_targets: dev
                || std::env::var("ALLOW_PRIVATE_TARGETS").is_ok_and(|v| v == "1"),
            workers: 4,
            delivery_timeout: Duration::from_secs(10),
            disable_after: 5,
            max_payload_bytes: 64 * 1024,
            data_dir: Some(
                std::env::var("DATA_DIR")
                    .unwrap_or_else(|_| "webhooks-data".into())
                    .into(),
            ),
            queue: QueueConfig::new(),
            max_attempts: 8,
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        })
    }
}

// ---- signatures ----

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `v1=` + hex HMAC-SHA256 over `"{timestamp}.{body}"`
pub fn sign(secret: &str, timestamp: u64, body: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!("v1={}", hex(&mac.finalize().into_bytes()))
}

#[allow(dead_code)] // for receivers (and the tests), not used by the dispatcher itself
/// What a receiver does: check the signature (constant time) and reject
/// timestamps outside `tolerance_secs` so that captured requests cannot be replayed later
pub fn verify_signature(
    secret: &str,
    timestamp_header: &str,
    body: &str,
    signature_header: &str,
    now: u64,
    tolerance_secs: u64,
) -> bool {
    let Ok(ts) = timestamp_header.trim().parse::<u64>() else {
        return false;
    };
    if now.abs_diff(ts) > tolerance_secs {
        return false;
    }
    constant_time_eq(
        sign(secret, ts, body).as_bytes(),
        signature_header.trim().as_bytes(),
    )
}

// ---- model ----

#[derive(Clone, Serialize, Deserialize)]
struct Endpoint {
    id: String,
    url: String,
    events: Vec<String>,
    secret: String,
    enabled: bool,
    disabled_reason: Option<String>,
    consecutive_failures: u32,
    created_at: u64,
}

impl Endpoint {
    fn subscribed_to(&self, event_type: &str) -> bool {
        self.events.iter().any(|e| e == "*" || e == event_type)
    }

    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "url": self.url,
            "events": self.events,
            "enabled": self.enabled,
            "disabled_reason": self.disabled_reason,
            "consecutive_failures": self.consecutive_failures,
            "created_at": self.created_at,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Delivery {
    endpoint_id: String,
    event_id: String,
    event_type: String,
    /// The exact JSON text that is signed and sent
    body: String,
}

#[derive(Clone, Serialize)]
struct LogEntry {
    event_id: String,
    attempt: u32,
    at: u64,
    status: Option<u16>,
    ok: bool,
    latency_ms: u64,
    error: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    endpoints: Vec<Endpoint>,
}

struct Hooks {
    registry: Mutex<Registry>,
    log: Mutex<HashMap<String, VecDeque<LogEntry>>>,
    idempotency: Mutex<VecDeque<(String, String)>>, // (key, event_id), oldest first
    queue: JobQueue<Delivery>,
    api_key: String,
    allow_private: bool,
    timeout: Duration,
    disable_after: u32,
    max_attempts: u32,
    data_file: Option<PathBuf>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Hooks {
    fn now(&self) -> u64 {
        (self.clock)()
    }

    async fn persist(&self) {
        let Some(file) = &self.data_file else { return };
        let Ok(json) = serde_json::to_vec_pretty(&*lock(&self.registry)) else {
            return;
        };
        let tmp = file.with_extension("tmp");
        if tokio::fs::write(&tmp, json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, file).await;
        }
    }

    fn endpoint(&self, id: &str) -> Option<Endpoint> {
        lock(&self.registry)
            .endpoints
            .iter()
            .find(|e| e.id == id)
            .cloned()
    }

    fn record(&self, endpoint_id: &str, entry: LogEntry) {
        let mut log = lock(&self.log);
        let list = log.entry(endpoint_id.to_string()).or_default();
        list.push_back(entry);
        while list.len() > LOG_PER_ENDPOINT {
            list.pop_front();
        }
    }

    /// Count a hopeless delivery; switch the endpoint off after too many in a row
    fn note_dead_delivery(&self, endpoint_id: &str) -> bool {
        let mut reg = lock(&self.registry);
        let Some(ep) = reg.endpoints.iter_mut().find(|e| e.id == endpoint_id) else {
            return false;
        };
        ep.consecutive_failures += 1;
        if ep.enabled && ep.consecutive_failures >= self.disable_after {
            ep.enabled = false;
            ep.disabled_reason = Some(format!(
                "{} deliveries in a row failed",
                ep.consecutive_failures
            ));
            return true;
        }
        false
    }

    fn note_success(&self, endpoint_id: &str) {
        if let Some(ep) = lock(&self.registry)
            .endpoints
            .iter_mut()
            .find(|e| e.id == endpoint_id)
        {
            ep.consecutive_failures = 0;
        }
    }
}

// ---- delivery ----

enum Attempt {
    Delivered(u16),
    Rejected(u16, Option<Duration>),
}

/// One signed POST to the vetted address
async fn post_once(
    addr: std::net::SocketAddr,
    host_header: &str,
    path: &str,
    headers: Vec<(&'static str, String)>,
    body: String,
) -> std::result::Result<Attempt, String> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|_| "connection failed".to_string())?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| "not an HTTP server".to_string())?;
    tokio::spawn(conn);
    let mut req = hyper::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", host_header);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let req = req
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| "invalid request".to_string())?;
    let res = sender
        .send_request(req)
        .await
        .map_err(|_| "no valid HTTP response".to_string())?;
    let status = res.status().as_u16();
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    Ok(if (200..300).contains(&status) {
        Attempt::Delivered(status)
    } else {
        Attempt::Rejected(status, retry_after)
    })
}

async fn deliver(hooks: Arc<Hooks>, job: Job<Delivery>) -> std::result::Result<(), JobError> {
    let d = job.payload;
    let Some(endpoint) = hooks.endpoint(&d.endpoint_id) else {
        return Ok(()); // deleted since: nothing to deliver to
    };
    if !endpoint.enabled {
        return Err(JobError::fatal("endpoint is disabled"));
    }
    let last_attempt = job.attempt >= hooks.max_attempts;
    let started = std::time::Instant::now();
    let log = |status: Option<u16>, ok: bool, error: Option<String>| {
        hooks.record(
            &d.endpoint_id,
            LogEntry {
                event_id: d.event_id.clone(),
                attempt: job.attempt,
                at: hooks.now(),
                status,
                ok,
                latency_ms: started.elapsed().as_millis() as u64,
                error,
            },
        );
    };
    // anything that ends this job for good
    let give_up = |msg: String| {
        if hooks.note_dead_delivery(&d.endpoint_id) {
            eprintln!(
                "webhooks: disabled endpoint {} after repeated failures",
                d.endpoint_id
            );
        }
        JobError::fatal(msg)
    };

    let uri: http::Uri = endpoint
        .url
        .parse()
        .map_err(|_| give_up("stored URL is invalid".into()))?;
    let host = uri.host().unwrap_or("").to_string();
    let port = uri.port_u16().unwrap_or(80);
    // vetted again on every attempt: DNS may have changed since registration
    let addr = match resolve_public(&host, port, hooks.allow_private).await {
        Ok(addr) => addr,
        Err(ResolveError::NotPublic(ip)) => {
            log(
                None,
                false,
                Some(format!("target address {ip} is not public")),
            );
            return Err(give_up(format!("target address {ip} is not public")));
        }
        Err(e) => {
            log(None, false, Some(e.to_string()));
            return Err(if last_attempt {
                give_up(e.to_string())
            } else {
                JobError::retry(e.to_string())
            });
        }
    };

    let ts = hooks.now();
    let headers = vec![
        ("content-type", "application/json".to_string()),
        ("user-agent", "ullun-webhooks/1".to_string()),
        ("x-webhook-id", d.event_id.clone()),
        ("x-webhook-event", d.event_type.clone()),
        ("x-webhook-timestamp", ts.to_string()),
        ("x-webhook-signature", sign(&endpoint.secret, ts, &d.body)),
        ("x-webhook-attempt", job.attempt.to_string()),
    ];
    let host_header = if port == 80 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();

    let outcome = match tokio::time::timeout(
        hooks.timeout,
        post_once(addr, &host_header, &path, headers, d.body.clone()),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err("timeout".to_string()),
    };

    match outcome {
        Ok(Attempt::Delivered(status)) => {
            log(Some(status), true, None);
            hooks.note_success(&d.endpoint_id);
            Ok(())
        }
        Ok(Attempt::Rejected(status, retry_after)) => {
            log(Some(status), false, Some(format!("HTTP {status}")));
            match status {
                410 => {
                    // the receiver says it is gone for good
                    let mut reg = lock(&hooks.registry);
                    if let Some(ep) = reg.endpoints.iter_mut().find(|e| e.id == d.endpoint_id) {
                        ep.enabled = false;
                        ep.disabled_reason = Some("receiver answered 410 Gone".into());
                    }
                    drop(reg);
                    Err(JobError::fatal("HTTP 410, endpoint disabled"))
                }
                // permanent client errors: sending the same thing again will not help
                400..=499 if !matches!(status, 408 | 425 | 429) => {
                    Err(give_up(format!("HTTP {status}")))
                }
                _ if last_attempt => Err(give_up(format!("HTTP {status}"))),
                _ => Err(match retry_after {
                    Some(after) => {
                        JobError::retry_after(format!("HTTP {status}"), after.min(MAX_RETRY_AFTER))
                    }
                    None => JobError::retry(format!("HTTP {status}")),
                }),
            }
        }
        Err(message) => {
            log(None, false, Some(message.clone()));
            Err(if last_attempt {
                give_up(message)
            } else {
                JobError::retry(message)
            })
        }
    }
}

// ---- handlers ----

fn hooks(req: &Request) -> Result<&Arc<Hooks>> {
    req.state::<Arc<Hooks>>()
}

async fn require_key(req: Request, next: Next) -> Result<Response> {
    let key = hooks(&req)?.api_key.clone();
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

#[derive(Deserialize)]
struct NewEndpoint {
    url: String,
    events: Vec<String>,
}

fn valid_event_name(name: &str) -> bool {
    name == "*"
        || ((1..=50).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_".contains(&b)))
}

async fn create_endpoint(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let body: NewEndpoint = req.json()?;

    let mut v = Validator::new();
    v.check(
        !body.events.is_empty() && body.events.len() <= 20,
        "events",
        "list 1 to 20 event names (or \"*\")",
    );
    v.check(
        body.events.iter().all(|e| valid_event_name(e)),
        "events",
        "names use a-z, 0-9, '.', '-' and '_' (50 characters at most)",
    );
    let uri = body
        .url
        .parse::<http::Uri>()
        .ok()
        .filter(|u| u.scheme_str() == Some("http") && u.host().is_some());
    v.check(
        uri.is_some(),
        "url",
        "must be an absolute http:// URL (this build has no TLS client)",
    );
    v.check(body.url.len() <= 2048, "url", "is too long");
    v.finish()?;
    let uri = uri.expect("validated");
    if let Err(e) = resolve_public(
        uri.host().unwrap_or(""),
        uri.port_u16().unwrap_or(80),
        h.allow_private,
    )
    .await
    {
        let mut v = Validator::new();
        v.error("url", e.to_string());
        v.finish()?;
    }

    let secret = random_token(32)?;
    let endpoint = {
        let mut reg = lock(&h.registry);
        if reg.endpoints.len() >= MAX_ENDPOINTS {
            return Err(Error::custom(
                409,
                format!("at most {MAX_ENDPOINTS} endpoints"),
            ));
        }
        let ep = Endpoint {
            id: format!("ep_{}", random_alphanumeric(12)?),
            url: body.url,
            events: body.events,
            secret: secret.clone(),
            enabled: true,
            disabled_reason: None,
            consecutive_failures: 0,
            created_at: h.now(),
        };
        reg.endpoints.push(ep.clone());
        ep
    };
    h.persist().await;
    // the secret is shown exactly once
    let mut view = endpoint.view();
    view["secret"] = json!(secret);
    Ok(Response::json(view)
        .with_header("location", format!("/api/endpoints/{}", endpoint.id))
        .with_status(http::StatusCode::CREATED))
}

async fn list_endpoints(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let items: Vec<Value> = lock(&h.registry)
        .endpoints
        .iter()
        .map(Endpoint::view)
        .collect();
    Ok(Response::json(json!({ "items": items })))
}

async fn get_endpoint(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let ep = h
        .endpoint(req.params.get("id")?)
        .ok_or_else(|| Error::not_found("no such endpoint"))?;
    Ok(Response::json(ep.view()))
}

async fn delete_endpoint(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let id = req.params.get("id")?;
    {
        let mut reg = lock(&h.registry);
        let before = reg.endpoints.len();
        reg.endpoints.retain(|e| e.id != id);
        if reg.endpoints.len() == before {
            return Err(Error::not_found("no such endpoint"));
        }
    }
    lock(&h.log).remove(id);
    h.persist().await;
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

async fn enable_endpoint(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let id = req.params.get("id")?;
    {
        let mut reg = lock(&h.registry);
        let ep = reg
            .endpoints
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| Error::not_found("no such endpoint"))?;
        ep.enabled = true;
        ep.disabled_reason = None;
        ep.consecutive_failures = 0;
    }
    h.persist().await;
    Ok(Response::json(
        h.endpoint(id).map(|e| e.view()).unwrap_or(Value::Null),
    ))
}

async fn rotate_secret(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let id = req.params.get("id")?;
    let secret = random_token(32)?;
    {
        let mut reg = lock(&h.registry);
        let ep = reg
            .endpoints
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| Error::not_found("no such endpoint"))?;
        ep.secret = secret.clone();
    }
    h.persist().await;
    Ok(Response::json(json!({ "id": id, "secret": secret })))
}

async fn deliveries(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let id = req.params.get("id")?;
    h.endpoint(id)
        .ok_or_else(|| Error::not_found("no such endpoint"))?;
    let items: Vec<LogEntry> = lock(&h.log)
        .get(id)
        .map(|l| l.iter().rev().cloned().collect())
        .unwrap_or_default();
    Ok(Response::json(json!({ "items": items })))
}

#[derive(Deserialize)]
struct NewEvent {
    #[serde(rename = "type")]
    event_type: String,
    data: Value,
}

async fn publish_event(req: Request) -> Result<Response> {
    let h = hooks(&req)?;
    let event: NewEvent = req.json()?;
    let mut v = Validator::new();
    v.check(
        valid_event_name(&event.event_type) && event.event_type != "*",
        "type",
        "use a-z, 0-9, '.', '-' and '_' (50 characters at most)",
    );
    v.finish()?;

    let idem = req
        .headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty());
    if idem.is_some_and(|k| k.len() > 100) {
        return Err(Error::bad_request(
            "idempotency-key is longer than 100 characters",
        ));
    }
    // a repeated publish (client retry) answers with the original event and queues nothing
    if let Some(key) = idem {
        if let Some((_, event_id)) = lock(&h.idempotency).iter().find(|(k, _)| k == key) {
            return Ok(Response::json(
                json!({ "event_id": event_id, "deliveries": 0, "duplicate": true }),
            )
            .with_status(http::StatusCode::ACCEPTED));
        }
    }

    let event_id = format!("evt_{}", random_alphanumeric(16)?);
    let created_at = h.now();
    let body = serde_json::to_string(&json!({ "id": event_id, "type": event.event_type, "created_at": created_at, "data": event.data }))
        .map_err(|_| Error::internal("event is not serializable"))?;

    let targets: Vec<String> = lock(&h.registry)
        .endpoints
        .iter()
        .filter(|e| e.enabled && e.subscribed_to(&event.event_type))
        .map(|e| e.id.clone())
        .collect();
    for endpoint_id in &targets {
        h.queue.enqueue_with(
            Delivery {
                endpoint_id: endpoint_id.clone(),
                event_id: event_id.clone(),
                event_type: event.event_type.clone(),
                body: body.clone(),
            },
            EnqueueOptions {
                key: Some(format!("{event_id}:{endpoint_id}")),
                ..Default::default()
            },
        );
    }
    if let Some(key) = idem {
        let mut seen = lock(&h.idempotency);
        seen.push_back((key.to_string(), event_id.clone()));
        while seen.len() > MAX_IDEMPOTENCY_KEYS {
            seen.pop_front();
        }
    }
    Ok(Response::json(
        json!({ "event_id": event_id, "deliveries": targets.len(), "duplicate": false }),
    )
    .with_status(http::StatusCode::ACCEPTED))
}

async fn queue_stats(req: Request) -> Result<Response> {
    Ok(Response::json(json!(hooks(&req)?.queue.stats())))
}

async fn dead_letters(req: Request) -> Result<Response> {
    let items: Vec<Value> = hooks(&req)?
        .queue
        .dead_jobs()
        .into_iter()
        .map(|j| json!({ "id": j.id, "endpoint_id": j.payload.endpoint_id, "event_id": j.payload.event_id, "event_type": j.payload.event_type, "attempts": j.attempts, "last_error": j.last_error }))
        .collect();
    Ok(Response::json(json!({ "items": items })))
}

async fn retry_dead(req: Request) -> Result<Response> {
    let id: JobId = req.params.parse("id")?;
    if hooks(&req)?.queue.retry_dead(id) {
        Ok(Response::new(http::StatusCode::ACCEPTED))
    } else {
        Err(Error::not_found("no such dead delivery"))
    }
}

async fn delete_dead(req: Request) -> Result<Response> {
    let id: JobId = req.params.parse("id")?;
    if hooks(&req)?.queue.delete_dead(id) {
        Ok(Response::new(http::StatusCode::NO_CONTENT))
    } else {
        Err(Error::not_found("no such dead delivery"))
    }
}

fn admin(summary: &str) -> Operation {
    Operation::new(summary)
        .tag("admin")
        .secured_by_api_key("x-api-key")
        .error_response(401, "Missing or invalid API key")
}

pub fn build(config: Config) -> App {
    let registry: Registry = config
        .data_dir
        .as_ref()
        .and_then(|d| {
            std::fs::create_dir_all(d)
                .ok()
                .map(|_| d.join("endpoints.json"))
        })
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let queue_cfg = match &config.data_dir {
        Some(d) => config.queue.clone().persist_to(d.join("queue.json")),
        None => config.queue.clone(),
    }
    .max_attempts(config.max_attempts);

    let hooks_state = Arc::new(Hooks {
        registry: Mutex::new(registry),
        log: Mutex::new(HashMap::new()),
        idempotency: Mutex::new(VecDeque::new()),
        queue: JobQueue::new(queue_cfg),
        api_key: config.api_key,
        allow_private: config.allow_private_targets,
        timeout: config.delivery_timeout,
        disable_after: config.disable_after.max(1),
        max_attempts: config.max_attempts.max(1),
        data_file: config.data_dir.as_ref().map(|d| d.join("endpoints.json")),
        clock: config.clock,
    });
    let worker_hooks = hooks_state.clone();
    let workers = config.workers;

    App::new()
        .state(hooks_state)
        .max_body_size(Some(config.max_payload_bytes))
        .background(move |ctx: Context| async move {
            let h = worker_hooks.clone();
            worker_hooks.queue.run(ctx.shutdown(), workers, move |job| deliver(h.clone(), job)).await;
        })
        .get("/healthz", |_req: Request| async { Ok(Response::json(json!({ "status": "ok" }))) })
        .group("/api", |g| {
            g.middleware(require_key)
                .post("/endpoints", create_endpoint)
                .doc(
                    "POST",
                    "/endpoints",
                    admin("Register an endpoint; the signing secret is returned once")
                        .request_json(schema::object([("url", schema::string()), ("events", schema::array(schema::string()))], ["url", "events"]))
                        .error_response(422, "Invalid URL or events, or target not public"),
                )
                .get("/endpoints", list_endpoints)
                .doc("GET", "/endpoints", admin("List endpoints (without secrets)"))
                .get("/endpoints/{id}", get_endpoint)
                .doc("GET", "/endpoints/{id}", admin("One endpoint").error_response(404, "No such endpoint"))
                .delete("/endpoints/{id}", delete_endpoint)
                .doc("DELETE", "/endpoints/{id}", admin("Delete an endpoint").response(204, "Deleted"))
                .post("/endpoints/{id}/enable", enable_endpoint)
                .doc("POST", "/endpoints/{id}/enable", admin("Switch a disabled endpoint back on"))
                .post("/endpoints/{id}/rotate-secret", rotate_secret)
                .doc("POST", "/endpoints/{id}/rotate-secret", admin("New signing secret, returned once"))
                .get("/endpoints/{id}/deliveries", deliveries)
                .doc("GET", "/endpoints/{id}/deliveries", admin("Recent delivery attempts, newest first"))
                .post("/events", publish_event)
                .doc(
                    "POST",
                    "/events",
                    admin("Publish an event to every subscribed endpoint; send an idempotency-key header to make retries safe")
                        .request_json(schema::object([("type", schema::string()), ("data", schema::object([], []))], ["type", "data"]))
                        .response(202, "Accepted for delivery"),
                )
                .get("/queue", queue_stats)
                .doc("GET", "/queue", admin("Queue counters"))
                .get("/queue/dead", dead_letters)
                .doc("GET", "/queue/dead", admin("Deliveries that ran out of attempts"))
                .post("/queue/dead/{id}/retry", retry_dead)
                .doc("POST", "/queue/dead/{id}/retry", admin("Try a dead delivery again"))
                .delete("/queue/dead/{id}", delete_dead)
                .doc("DELETE", "/queue/dead/{id}", admin("Forget a dead delivery"))
        })
        .openapi(OpenApiInfo::new("Webhooks", "1.0.0").swagger_ui("/docs"))
}
