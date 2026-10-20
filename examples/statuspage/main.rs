//! Status page and uptime monitor
//!
//! Checks HTTP and TCP targets in the background, keeps a history, and serves a
//! public status page that updates live (Server-Sent Events), a JSON API, and an
//! API-key protected admin API for managing monitors. Stops cleanly on CTRL+C /
//! SIGTERM: the event streams end, running checks finish.
//!
//! Configuration (environment):
//!   PORT                  listen port (default 3000)
//!   ADMIN_KEY             key for the admin API (required unless DEV=1)
//!   ALLOW_PRIVATE_TARGETS 1 to allow checking loopback/private/link-local addresses
//!                         (off by default: SSRF protection for the admin API)
//!   DEV                   1 = insecure demo key and private targets allowed
//!
//! Run:   DEV=1 cargo run --example statuspage      then open http://localhost:3000
//! Try:   curl -s -X POST localhost:3000/api/monitors -H 'x-api-key: dev-admin-key' \
//!             -H 'content-type: application/json' \
//!             -d '{"name":"This service","target":"http://127.0.0.1:3000/healthz","interval_secs":5}'
//!        curl -N localhost:3000/events

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
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
