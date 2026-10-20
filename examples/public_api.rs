//! Scenario: public JSON API with API keys, rate limiting and CORS
//!
//! - `x-api-key` authentication middleware (keys map to a client name)
//! - fixed-window rate limit per key, with `retry-after` and `x-ratelimit-*`
//! - CORS for browser clients
//! - versioned route group `/v1` with query-string pagination
//!
//! In production put TLS and a coarse IP-based limiter (nginx/Caddy) in front.
//!
//! Port: set PORT (default 3000)
//! Run:  cargo run --example public_api
//! Try:  curl -H 'x-api-key: demo-key' 'localhost:3000/v1/books?page=2&per_page=3'
//!       curl -i localhost:3000/v1/books          # 401

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ullun::prelude::*;

const LIMIT: u32 = 5;
const WINDOW: Duration = Duration::from_secs(10);

struct Window {
    started: Instant,
    count: u32,
}

#[derive(Serialize)]
struct Book {
    id: u32,
    title: String,
}

#[tokio::main]
async fn main() {
    let keys: Arc<HashMap<&'static str, &'static str>> = Arc::new(HashMap::from([
        ("demo-key", "demo-client"),
        ("other-key", "other-client"),
    ]));
    let windows: Arc<Mutex<HashMap<&'static str, Window>>> = Arc::default();

    let auth_and_limit = move |req: Request, next: Next| {
        let keys = keys.clone();
        let windows = windows.clone();
        async move {
            // CORS preflight carries no credentials
            if req.method == http::Method::OPTIONS {
                return next.run(req).await;
            }
            let client = req
                .headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .and_then(|k| keys.get(k).copied())
                .ok_or_else(|| Error::unauthorized("missing or unknown x-api-key"))?;

            let (allowed, remaining, reset) = {
                let mut map = windows.lock().expect("rate limit lock poisoned");
                let w = map.entry(client).or_insert(Window {
                    started: Instant::now(),
                    count: 0,
                });
                if w.started.elapsed() >= WINDOW {
                    *w = Window {
                        started: Instant::now(),
                        count: 0,
                    };
                }
                let reset = WINDOW.saturating_sub(w.started.elapsed()).as_secs().max(1);
                if w.count < LIMIT {
                    w.count += 1;
                    (true, LIMIT - w.count, reset)
                } else {
                    (false, 0, reset)
                }
            };

            if !allowed {
                return Ok(
                    Response::from_error(Error::custom(429, "rate limit exceeded"))
                        .with_header("retry-after", reset.to_string())
                        .with_header("x-ratelimit-remaining", "0"),
                );
            }
            let res = next.run(req).await?;
            Ok(res
                .with_header("x-ratelimit-limit", LIMIT.to_string())
                .with_header("x-ratelimit-remaining", remaining.to_string())
                .with_header("x-ratelimit-reset", reset.to_string()))
        }
    };

    let catalog: Arc<Vec<Book>> = Arc::new(
        (1..=23)
            .map(|id| Book {
                id,
                title: format!("Book #{id}"),
            })
            .collect(),
    );

    let list = {
        let catalog = catalog.clone();
        move |req: Request| {
            let catalog = catalog.clone();
            async move {
                let page: usize = req
                    .query
                    .get("page")
                    .unwrap_or("1")
                    .parse()
                    .unwrap_or(1)
                    .max(1);
                let per_page: usize = req
                    .query
                    .get("per_page")
                    .unwrap_or("10")
                    .parse()
                    .unwrap_or(10)
                    .clamp(1, 50);
                let items: Vec<&Book> = catalog
                    .iter()
                    .skip((page - 1) * per_page)
                    .take(per_page)
                    .collect();
                Ok(Response::json(serde_json::json!({
                    "page": page,
                    "per_page": per_page,
                    "total": catalog.len(),
                    "items": items,
                })))
            }
        }
    };

    // CORS wraps everything registered after it, including auth failures
    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );

    App::new()
        .middleware(ullun::middleware::cors(vec![
            "https://app.example.com".to_string()
        ]))
        .middleware(auth_and_limit)
        .group("/v1", |g| {
            g.get("/books", list).get("/ping", |_req: Request| async {
                Ok(Response::text("pong"))
            })
        })
        .run(&addr)
        .await
        .unwrap();
}
