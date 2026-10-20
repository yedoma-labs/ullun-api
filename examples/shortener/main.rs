//! URL shortener service
//!
//! A small but complete service: API-key protected management API, public
//! redirects, click counting, expiry, rate limiting, persistence to a JSON
//! file, health check and a generated OpenAPI document.
//!
//! Configuration (environment):
//!   PORT            listen port (default 3000)
//!   API_KEYS        comma separated keys allowed to manage links (default: demo-key)
//!   BASE_URL        public URL used in responses (default http://localhost:PORT)
//!   DATA_FILE       JSON file for persistence (default: in-memory only)
//!
//! Run:   cargo run --example shortener
//! Try:   curl -s -X POST localhost:3000/api/links -H 'x-api-key: demo-key' \
//!             -H 'content-type: application/json' -d '{"url":"https://example.com/docs"}'
//!        curl -i localhost:3000/<code>
//!        curl -s -H 'x-api-key: demo-key' localhost:3000/api/links/<code>
//!        curl localhost:3000/openapi.json

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let config = app::Config::from_env(&port);
    if config.api_keys.iter().any(|k| k == "demo-key") {
        eprintln!("warning: using the demo API key, set API_KEYS for anything real");
    }
    app::build(config)
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
