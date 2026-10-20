//! File drive: upload, store, share and download files
//!
//! Multipart uploads with per-user quotas, files stored on disk under generated
//! names (never the client's), downloads with resumable ranges, time- and
//! count-limited share links, safe handling of risky file types, a background
//! cleanup task and an OpenAPI document.
//!
//! Configuration (environment):
//!   PORT         listen port (default 3000)
//!   DATA_DIR     where blobs and the index live (default ./drive-data)
//!   DRIVE_USERS  `name:api-key:quota-mb,...` (required unless DEV=1)
//!   DEV          1 = demo users alice/bob with demo keys
//!
//! Run:   DEV=1 cargo run --example drive
//! Try:   curl -s -H 'x-api-key: alice-demo-key-0123456789' -F file=@README.md -F description=docs \
//!             localhost:3000/api/upload
//!        curl -s -H 'x-api-key: alice-demo-key-0123456789' -X POST localhost:3000/api/files/<id>/share \
//!             -H 'content-type: application/json' -d '{"ttl_secs":3600,"max_downloads":5}'
//!        curl -OJ localhost:3000/s/<token>

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
    let base = std::env::var("BASE_URL").unwrap_or_else(|_| format!("http://localhost:{port}"));
    let app = match app::build(app::Config {
        base_url: base,
        ..config
    })
    .await
    {
        Ok(app) => app,
        Err(e) => {
            eprintln!("cannot prepare the data directory: {e}");
            std::process::exit(1);
        }
    };
    app.run(&format!("0.0.0.0:{port}")).await.unwrap();
}
