//! API gateway / reverse proxy
//!
//! Routes path prefixes to groups of upstream servers: round-robin balancing,
//! retries of idempotent requests on another upstream, passive health checks
//! (an upstream that keeps failing is skipped for a while, then probed again),
//! timeouts (504) and connection errors (502), streamed responses, forwarded
//! headers, per-client rate limiting, request ids, JSON access log and
//! Prometheus metrics for the operator.
//!
//! Configuration (environment):
//!   PORT                listen port (default 8080)
//!   GATEWAY_ROUTES      `prefix=upstream[,upstream...];prefix=...`, e.g.
//!                       `/api/users=http://10.0.0.5:9000,http://10.0.0.6:9000;/api/orders=http://10.0.0.7:9000`
//!   GATEWAY_ADMIN_KEY   key for /_gateway/status and /_gateway/metrics (required unless DEV=1)
//!   DEV                 1 = demo route /echo -> http://127.0.0.1:9001 and key "dev-admin-key"
//!
//! Run:   DEV=1 cargo run --example gateway          (then start something on :9001)
//! Try:   curl -i localhost:8080/echo/hello
//!        curl -H 'x-api-key: dev-admin-key-dev-admin-key' localhost:8080/_gateway/status

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let config = match app::Config::from_env() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("configuration error: {message}");
            std::process::exit(2);
        }
    };
    app::build(config)
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
