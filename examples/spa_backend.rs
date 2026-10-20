//! Scenario: single-page app + JSON backend in one binary
//!
//! - static frontend served from a directory (`/` -> index.html, assets)
//! - JSON API under `/api`, registered before the static catch-all
//! - security headers on every response
//!
//! The example writes a tiny frontend into a temp directory so it runs
//! without a build step; point `serve_static` at your `dist/` instead.
//!
//! Port: set PORT (default 3000)
//! Run:  cargo run --example spa_backend   then open http://localhost:3000

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use ullun::prelude::*;

const INDEX: &str = r#"<!doctype html>
<meta charset="utf-8"><title>ullun SPA</title>
<link rel="stylesheet" href="/app.css">
<h1>Clicks: <span id="n">…</span></h1>
<button id="b">+1</button>
<script src="/app.js"></script>"#;

const CSS: &str = "body{font-family:system-ui;margin:3rem}button{font-size:1.2rem}";

const JS: &str = r#"
const n = document.getElementById('n');
const show = async r => n.textContent = (await r.json()).clicks;
fetch('/api/clicks').then(show);
document.getElementById('b').onclick = () => fetch('/api/clicks', {method: 'POST'}).then(show);
"#;

async fn security_headers(req: Request, next: Next) -> Result<Response> {
    let res = next.run(req).await?;
    Ok(res
        .with_header("x-content-type-options", "nosniff")
        .with_header("x-frame-options", "DENY")
        .with_header("referrer-policy", "same-origin")
        .with_header(
            "content-security-policy",
            "default-src 'self'; style-src 'self' 'unsafe-inline'",
        ))
}

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join("ullun-spa-example");
    std::fs::create_dir_all(&dir).expect("create frontend dir");
    std::fs::write(dir.join("index.html"), INDEX).unwrap();
    std::fs::write(dir.join("app.css"), CSS).unwrap();
    std::fs::write(dir.join("app.js"), JS).unwrap();

    let clicks = Arc::new(AtomicU64::new(0));
    let (read, write) = (clicks.clone(), clicks);

    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );

    App::new()
        .middleware(security_headers)
        .group("/api", |g| {
            g.get("/clicks", move |_req: Request| {
                let read = read.clone();
                async move {
                    Ok(Response::json(
                        serde_json::json!({ "clicks": read.load(Ordering::Relaxed) }),
                    ))
                }
            })
            .post("/clicks", move |_req: Request| {
                let write = write.clone();
                async move {
                    let clicks = write.fetch_add(1, Ordering::Relaxed) + 1;
                    Ok(Response::json(serde_json::json!({ "clicks": clicks })))
                }
            })
        })
        // static assets last: "/" -> index.html, everything else by file name
        .serve_static("/", dir.to_str().expect("utf-8 temp dir"))
        .run(&addr)
        .await
        .unwrap();
}
