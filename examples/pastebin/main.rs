//! Pastebin: share text snippets by link
//!
//! Web form and JSON API, HTML and raw views, expiry, burn-after-read, delete
//! tokens, size limits, a storage cap and rate limiting on creation.
//!
//! Configuration (environment):
//!   PORT            listen port (default 3000)
//!   BASE_URL        public URL used in links (default http://localhost:PORT)
//!
//! Run:   cargo run --example pastebin   then open http://localhost:3000
//! Try:   curl -s -X POST localhost:3000/pastes -H 'content-type: application/json' \
//!             -d '{"content":"hello\nworld","ttl_secs":3600,"burn_after_read":false}'
//!        curl localhost:3000/raw/<id>
//!        curl -X DELETE -H 'x-delete-token: <token>' localhost:3000/pastes/<id>

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    app::build(app::Config::from_env(&port))
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
