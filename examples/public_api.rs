//! Scenario: public JSON API with API keys, rate limiting, CORS and OpenAPI
//!
//! - `x-api-key` authentication middleware (keys map to a client name)
//! - per-key rate limit with `retry-after` and `x-ratelimit-*` (`RateLimiter`)
//! - CORS for browser clients
//! - versioned route group `/v1` with query-string pagination
//! - generated OpenAPI document at `/openapi.json` (passes through the same middleware)
//!
//! In production put TLS and a coarse IP-based limiter (nginx/Caddy) in front.
//!
//! Port: set PORT (default 3000)
//! Run:  cargo run --example public_api
//! Try:  curl -H 'x-api-key: demo-key' 'localhost:3000/v1/books?page=2&per_page=3'
//!       curl -i localhost:3000/v1/books          # 401
//!       curl -H 'x-api-key: demo-key' localhost:3000/openapi.json   # behind the same auth

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;

#[derive(Serialize)]
struct Book {
    id: u32,
    title: String,
}

#[derive(Deserialize)]
struct Page {
    page: Option<usize>,
    per_page: Option<usize>,
}

fn api_key(req: &Request) -> Option<String> {
    req.headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[tokio::main]
async fn main() {
    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );

    let keys: Arc<HashMap<&'static str, &'static str>> = Arc::new(HashMap::from([
        ("demo-key", "demo-client"),
        ("other-key", "other-client"),
    ]));
    let auth = move |req: Request, next: Next| {
        let keys = keys.clone();
        async move {
            // CORS preflight carries no credentials
            if req.method != http::Method::OPTIONS {
                api_key(&req)
                    .filter(|k| keys.contains_key(k.as_str()))
                    .ok_or_else(|| Error::unauthorized("missing or unknown x-api-key"))?;
            }
            next.run(req).await
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
                let q: Page = req.query.parse()?;
                let page = q.page.unwrap_or(1).max(1);
                let per_page = q.per_page.unwrap_or(10).clamp(1, 50);
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

    // Order matters: CORS wraps auth failures, auth runs before the limiter
    App::new()
        .middleware(ullun::cors::Cors::new().allow_origin("https://app.example.com"))
        .middleware(auth)
        .middleware(RateLimiter::new(5, Duration::from_secs(10)).key_by(api_key))
        .group("/v1", |g| {
            g.get("/books", list)
                .doc(
                    "GET",
                    "/books",
                    Operation::new("List books")
                        .tag("books")
                        .query_param("page", schema::integer(), "1-based page", false)
                        .query_param("per_page", schema::integer(), "1 to 50, default 10", false)
                        .response_json(
                            200,
                            "A page of books",
                            schema::object(
                                [
                                    ("page", schema::integer()),
                                    ("total", schema::integer()),
                                    (
                                        "items",
                                        schema::array(schema::object(
                                            [
                                                ("id", schema::integer()),
                                                ("title", schema::string()),
                                            ],
                                            ["id", "title"],
                                        )),
                                    ),
                                ],
                                ["page", "total", "items"],
                            ),
                        )
                        .error_response(401, "Missing or unknown API key")
                        .error_response(429, "Rate limit exceeded"),
                )
                .get("/ping", |_req: Request| async {
                    Ok(Response::text("pong"))
                })
        })
        .openapi(
            OpenApiInfo::new("Books API", "1.0.0")
                .description("Demo API protected by API keys (x-api-key header)"),
        )
        .run(&addr)
        .await
        .unwrap();
}
