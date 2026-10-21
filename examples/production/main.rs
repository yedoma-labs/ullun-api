//! Production-ready service skeleton
//!
//! Wires the pieces that matter outside a laptop in a sensible order: tracing,
//! logging and metrics outside, security headers and CORS around every response,
//! load shedding before work starts, auth, then per-route behaviour (cache,
//! idempotency). Copy it and replace the handlers.
//!
//! Configuration (environment):
//!   PORT              listen port (default 3000)
//!   TRUSTED_PROXIES   comma separated networks of your load balancers (default: none)
//!   ADMIN_KEY         API key for /admin (default: a random one is printed)
//!   SHOP_ORIGIN       origin allowed to call the API from a browser (default http://localhost:5173)
//!
//! Run:   cargo run --example production
//! Try:   curl -s localhost:3000/api/catalog -i          (X-Cache: MISS, then HIT)
//!        curl -s -X POST localhost:3000/api/orders -H 'idempotency-key: 1' -d '{"sku":"mug"}'
//!        curl -s localhost:3000/readyz
//!        curl -s localhost:3000/metrics

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let config = app::Config::from_env();
    if std::env::var("ADMIN_KEY").is_err() {
        eprintln!(
            "ADMIN_KEY not set, using a random one: {}",
            config.admin_key
        );
    }
    app::build(config)
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
