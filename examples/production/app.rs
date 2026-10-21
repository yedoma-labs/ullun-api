//! Production skeleton: application wiring, shared by `main.rs` and the tests

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ullun::auth::ApiKeyAuth;
use ullun::cache::ResponseCache;
use ullun::cors::Cors;
use ullun::health::Health;
use ullun::idempotency::Idempotency;
use ullun::limits::{ConcurrencyLimit, Timeout};
use ullun::observability::{AccessLog, Metrics, RequestIdLayer};
use ullun::prelude::*;
use ullun::problem::ProblemDetails;
use ullun::proxy::TrustedProxies;
use ullun::rate_limit::RateLimiter;
use ullun::security::random_token;
use ullun::security_headers::{Csp, SecurityHeaders};
use ullun::trace::TraceLayer;

pub struct Config {
    pub shop_origin: String,
    pub admin_key: String,
    pub trusted_proxies: TrustedProxies,
    /// Where access log lines go
    pub log_sink: Arc<dyn Fn(&str) + Send + Sync>,
    pub shutdown_delay: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let mut proxies = TrustedProxies::new();
        for net in std::env::var("TRUSTED_PROXIES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            proxies = proxies
                .trust(net)
                .unwrap_or_else(|e| panic!("TRUSTED_PROXIES: {e}"));
        }
        Self {
            shop_origin: std::env::var("SHOP_ORIGIN")
                .unwrap_or_else(|_| "http://localhost:5173".into()),
            admin_key: std::env::var("ADMIN_KEY")
                .unwrap_or_else(|_| random_token(16).expect("OS randomness")),
            trusted_proxies: proxies,
            log_sink: Arc::new(|line| println!("{line}")),
            shutdown_delay: Duration::from_secs(5),
        }
    }
}

struct Orders(AtomicU64);

async fn catalog(_req: Request) -> Result<Response> {
    // imagine an expensive query; the cache runs it at most once per 30 s
    Ok(Response::json(serde_json::json!([
        { "sku": "mug", "price_cents": 1200 },
        { "sku": "shirt", "price_cents": 2500 },
    ])))
}

async fn place_order(req: Request) -> Result<Response> {
    #[derive(serde::Deserialize)]
    struct NewOrder {
        sku: String,
    }
    let order: NewOrder = req.json()?;
    let id = req.state::<Orders>()?.0.fetch_add(1, Ordering::SeqCst) + 1;
    Ok(
        Response::json(serde_json::json!({ "id": id, "sku": order.sku }))
            .with_status(http::StatusCode::CREATED),
    )
}

async fn whoami(req: Request) -> Result<Response> {
    Ok(Response::json(serde_json::json!({
        "client_ip": req.client_ip().map(|i| i.to_string()),
        "secure": req.is_secure(),
        "trace_id": req.trace().map(|t| t.trace_id.clone()),
    })))
}

async fn admin_stats(req: Request) -> Result<Response> {
    Ok(Response::json(serde_json::json!({
        "orders": req.state::<Orders>()?.0.load(Ordering::SeqCst),
        "caller": req.principal(),
    })))
}

pub fn build(config: Config) -> App {
    let metrics = Metrics::new();
    let health = Health::new().check("self", || async { Ok::<(), String>(()) });
    let sink = config.log_sink.clone();

    App::new()
        .state(Orders(AtomicU64::new(0)))
        // slowloris, connection floods, oversized bodies
        .header_read_timeout(Duration::from_secs(10))
        .max_connections(10_000)
        .max_body_size(Some(64 * 1024))
        .trusted_proxies(config.trusted_proxies)
        .shutdown_delay(config.shutdown_delay)
        // 1. see everything, including rejections
        .middleware(TraceLayer::new())
        .middleware(RequestIdLayer::new())
        .middleware(AccessLog::new().json().sink(move |line| sink(line)))
        .middleware(metrics.clone())
        // 2. decorate every response, errors included
        .middleware(SecurityHeaders::new().csp(Csp::api()))
        .middleware(
            Cors::new()
                .allow_origin(config.shop_origin)
                .expose_headers(["x-request-id", "idempotency-replayed"]),
        )
        .middleware(ProblemDetails::new().type_base("https://api.example.com/problems/"))
        // 3. shed load before doing work
        .middleware(Timeout::new(Duration::from_secs(10)))
        .middleware(ConcurrencyLimit::new(512))
        .middleware(RateLimiter::new(120, Duration::from_secs(60)).sliding_window())
        // probes and metrics
        .get("/livez", health.liveness())
        .get("/readyz", health.readiness())
        .get("/metrics", metrics.handler())
        .get("/whoami", whoami)
        // public API with per-route behaviour
        .group("/api", |g| {
            g.middleware(
                ResponseCache::new()
                    .ttl(Duration::from_secs(30))
                    .stale_while_revalidate(Duration::from_secs(120)),
            )
            // retried POSTs run once; the key is required here
            .middleware(Idempotency::new().required())
            .get("/catalog", catalog)
            .post("/orders", place_order)
        })
        // internal API behind a key
        .group("/admin", |g| {
            g.middleware(ApiKeyAuth::new().key("ops", &config.admin_key))
                .get("/stats", admin_stats)
        })
        .fallback(|_req: Request| async {
            Err::<Response, _>(Error::not_found("No such resource"))
        })
}
