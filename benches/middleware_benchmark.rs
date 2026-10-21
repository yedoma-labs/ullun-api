//! Cost of the built-in middleware, measured in-process with `TestClient`
//! (no sockets): routing + middleware + handler + response conversion.
//!
//! `cargo bench --bench middleware_benchmark`

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::time::Duration;
use ullun::cache::ResponseCache;
use ullun::cors::Cors;
use ullun::idempotency::Idempotency;
use ullun::limits::{ConcurrencyLimit, Timeout};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::security_headers::{Csp, SecurityHeaders};
use ullun::testing::TestClient;

fn base() -> App {
    App::new()
        .get("/", |_req: Request| async { Ok(Response::text("hello")) })
        .post("/", |_req: Request| async { Ok(Response::text("created")) })
}

fn bench_get(c: &mut Criterion, rt: &tokio::runtime::Runtime, name: &str, app: App) {
    let client = TestClient::new(app);
    c.bench_function(name, |b| {
        b.to_async(rt)
            .iter(|| async { black_box(client.get("/").send().await) })
    });
}

fn middleware(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    bench_get(c, &rt, "mw/none", base());
    bench_get(
        c,
        &rt,
        "mw/security_headers",
        base().middleware(SecurityHeaders::new()),
    );
    bench_get(
        c,
        &rt,
        "mw/security_headers+csp_nonce",
        base().middleware(
            SecurityHeaders::new().csp(Csp::web().script_src(["'self'", "'nonce-{nonce}'"])),
        ),
    );
    bench_get(
        c,
        &rt,
        "mw/rate_limiter",
        base().middleware(RateLimiter::new(u32::MAX, Duration::from_secs(60))),
    );
    bench_get(
        c,
        &rt,
        "mw/timeout+concurrency_limit",
        base()
            .middleware(Timeout::new(Duration::from_secs(10)))
            .middleware(ConcurrencyLimit::new(1024)),
    );
    bench_get(
        c,
        &rt,
        "mw/response_cache_hit",
        base().middleware(ResponseCache::new().ttl(Duration::from_secs(3600))),
    );

    // CORS: simple request from an allowed origin, and a preflight
    let cors =
        TestClient::new(base().middleware(Cors::new().allow_origin("https://app.example.com")));
    c.bench_function("mw/cors_simple_request", |b| {
        b.to_async(&rt).iter(|| async {
            black_box(
                cors.get("/")
                    .header("origin", "https://app.example.com")
                    .send()
                    .await,
            )
        })
    });
    c.bench_function("mw/cors_preflight", |b| {
        b.to_async(&rt).iter(|| async {
            black_box(
                cors.options("/")
                    .header("origin", "https://app.example.com")
                    .header("access-control-request-method", "POST")
                    .send()
                    .await,
            )
        })
    });

    // Idempotency: replaying a stored response vs the first run
    let idem = TestClient::new(base().middleware(Idempotency::new()));
    rt.block_on(idem.post("/").header("idempotency-key", "warm").send());
    c.bench_function("mw/idempotency_replay", |b| {
        b.to_async(&rt).iter(|| async {
            black_box(
                idem.post("/")
                    .header("idempotency-key", "warm")
                    .send()
                    .await,
            )
        })
    });
}

criterion_group!(benches, middleware);
criterion_main!(benches);
