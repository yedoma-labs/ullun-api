//! The URL shortener example, tested over real HTTP

mod common;

// from_env / system_now are only used by the example binary
#[allow(dead_code)]
#[path = "../examples/shortener/app.rs"]
mod shortener;

use common::{spawn, Server};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const KEY: &str = "test-key";

struct Fixture {
    server: Server,
    clock: Arc<AtomicU64>,
}

fn config(clock: Arc<AtomicU64>, limit: u32) -> shortener::Config {
    let c = clock.clone();
    shortener::Config {
        api_keys: vec![KEY.into(), "second-key".into()],
        base_url: "https://sho.rt/".into(),
        data_file: None,
        rate_limit: (limit, Duration::from_secs(60)),
        clock: Arc::new(move || c.load(Ordering::SeqCst)),
    }
}

async fn fixture() -> Fixture {
    fixture_with(1000, 1000).await
}

async fn fixture_with(start: u64, limit: u32) -> Fixture {
    let clock = Arc::new(AtomicU64::new(start));
    let server = spawn(shortener::build(config(clock.clone(), limit))).await;
    Fixture { server, clock }
}

impl Fixture {
    async fn create(&self, body: serde_json::Value) -> common::Reply {
        self.server
            .post("/api/links")
            .header("x-api-key", KEY)
            .json(&body)
            .send()
            .await
    }
}

#[tokio::test]
async fn create_follow_and_count_clicks() {
    let f = fixture().await;

    let created = f
        .create(json!({ "url": "https://example.com/docs?a=1" }))
        .await;
    assert_eq!(created.status, 201);
    let link = created.json();
    let code = link["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 7);
    assert_eq!(link["short_url"], format!("https://sho.rt/{code}"));
    assert_eq!(
        created.header("location"),
        format!("https://sho.rt/api/links/{code}")
    );
    assert_eq!(link["clicks"], 0);

    for _ in 0..3 {
        let hop = f.server.get(&format!("/{code}")).send().await;
        assert_eq!(hop.status, 302);
        assert_eq!(hop.header("location"), "https://example.com/docs?a=1");
        assert_eq!(hop.header("cache-control"), "no-store");
        assert_eq!(hop.header("referrer-policy"), "no-referrer");
    }

    let stats = f
        .server
        .get(&format!("/api/links/{code}"))
        .header("x-api-key", KEY)
        .send()
        .await;
    assert_eq!(stats.status, 200);
    assert_eq!(stats.json()["clicks"], 3);
}

#[tokio::test]
async fn custom_codes_conflicts_and_reserved_names() {
    let f = fixture().await;

    let ok = f
        .create(json!({ "url": "https://a.example", "code": "my-link_1" }))
        .await;
    assert_eq!(ok.status, 201);
    assert_eq!(f.server.get("/my-link_1").send().await.status, 302);

    let dup = f
        .create(json!({ "url": "https://b.example", "code": "my-link_1" }))
        .await;
    assert_eq!(dup.status, 409);
    // the original is untouched
    assert_eq!(
        f.server.get("/my-link_1").send().await.header("location"),
        "https://a.example"
    );

    for bad in [
        "ab",
        "has space",
        "slash/ed",
        "ümlaut",
        &"x".repeat(33),
        "api",
        "HEALTHZ",
        "docs",
    ] {
        let res = f
            .create(json!({ "url": "https://a.example", "code": bad }))
            .await;
        assert_eq!(res.status, 400, "code {bad:?}");
    }
}

#[tokio::test]
async fn only_http_urls_are_accepted() {
    let f = fixture().await;
    let long = format!("https://example.com/{}", "a".repeat(2100));
    for bad in [
        "javascript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "ftp://example.com/file",
        "//example.com",
        "/relative/path",
        "https://",
        "https://exa mple.com",
        "https://example.com/\nSet-Cookie: x=1",
        "not a url",
        "",
        long.as_str(),
    ] {
        let res = f.create(json!({ "url": bad })).await;
        assert_eq!(res.status, 400, "url {bad:?}");
    }
    for good in [
        "http://example.com",
        "https://example.com:8443/a/b?c=d#e",
        "https://user@example.com",
    ] {
        assert_eq!(
            f.create(json!({ "url": good })).await.status,
            201,
            "url {good:?}"
        );
    }
    // malformed bodies
    let res = f
        .server
        .post("/api/links")
        .header("x-api-key", KEY)
        .text("url=https://a.example")
        .send()
        .await;
    assert_eq!(res.status, 400);
    assert_eq!(f.create(json!({ "nope": 1 })).await.status, 400);
}

#[tokio::test]
async fn ttl_is_validated_and_links_expire() {
    let f = fixture_with(5_000, 1000).await;

    for bad in [0u64, 31 * 24 * 3600 + 1] {
        assert_eq!(
            f.create(json!({ "url": "https://a.example", "ttl_secs": bad }))
                .await
                .status,
            400
        );
    }

    let created = f
        .create(json!({ "url": "https://a.example", "code": "temp", "ttl_secs": 60 }))
        .await;
    assert_eq!(created.json()["expires_at"], 5_060);

    f.clock.store(5_059, Ordering::SeqCst);
    assert_eq!(f.server.get("/temp").send().await.status, 302);

    f.clock.store(5_060, Ordering::SeqCst);
    assert_eq!(f.server.get("/temp").send().await.status, 410);
    // gone for good, and the code is free again
    assert_eq!(f.server.get("/temp").send().await.status, 404);
    let again = f
        .create(json!({ "url": "https://b.example", "code": "temp" }))
        .await;
    assert_eq!(again.status, 201);
}

#[tokio::test]
async fn expired_links_are_hidden_from_the_api() {
    let f = fixture_with(100, 1000).await;
    f.create(json!({ "url": "https://a.example", "code": "short-lived", "ttl_secs": 10 }))
        .await;
    f.create(json!({ "url": "https://b.example", "code": "forever" }))
        .await;
    f.clock.store(200, Ordering::SeqCst);

    let get = f
        .server
        .get("/api/links/short-lived")
        .header("x-api-key", KEY)
        .send()
        .await;
    assert_eq!(get.status, 404);
    let list = f
        .server
        .get("/api/links")
        .header("x-api-key", KEY)
        .send()
        .await
        .json();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["code"], "forever");
}

#[tokio::test]
async fn management_api_needs_a_valid_key() {
    let f = fixture().await;
    for (method, path) in [
        ("GET", "/api/links"),
        ("POST", "/api/links"),
        ("GET", "/api/links/x"),
        ("DELETE", "/api/links/x"),
    ] {
        let none = f.server.request(method, path).send().await;
        assert_eq!(none.status, 401, "{method} {path}");
        let wrong = f
            .server
            .request(method, path)
            .header("x-api-key", "nope")
            .send()
            .await;
        assert_eq!(wrong.status, 401, "{method} {path}");
        let prefix = f
            .server
            .request(method, path)
            .header("x-api-key", "test-ke")
            .send()
            .await;
        assert_eq!(prefix.status, 401, "{method} {path}");
    }
    // any configured key works
    let second = f
        .server
        .get("/api/links")
        .header("x-api-key", "second-key")
        .send()
        .await;
    assert_eq!(second.status, 200);
    // the public side needs none
    assert_eq!(f.server.get("/healthz").send().await.status, 200);
}

#[tokio::test]
async fn rate_limit_is_per_key_and_ignores_unauthenticated_calls() {
    let f = fixture_with(1, 3).await;

    // junk requests must not use up anyone's budget
    for _ in 0..10 {
        assert_eq!(f.server.get("/api/links").send().await.status, 401);
    }
    for i in 0..3 {
        let res = f
            .server
            .get("/api/links")
            .header("x-api-key", KEY)
            .send()
            .await;
        assert_eq!(res.status, 200, "request {i}");
    }
    let limited = f
        .server
        .get("/api/links")
        .header("x-api-key", KEY)
        .send()
        .await;
    assert_eq!(limited.status, 429);
    assert!(limited.header("retry-after").parse::<u64>().unwrap() >= 1);
    // another key has its own budget
    let other = f
        .server
        .get("/api/links")
        .header("x-api-key", "second-key")
        .send()
        .await;
    assert_eq!(other.status, 200);
    // public redirects are not limited
    for _ in 0..10 {
        assert_eq!(f.server.get("/nothing").send().await.status, 404);
    }
}

#[tokio::test]
async fn list_is_paginated_newest_first_and_delete_works() {
    let f = fixture_with(100, 1000).await;
    for (i, code) in ["aaa", "bbb", "ccc", "ddd", "eee"].iter().enumerate() {
        f.clock.store(100 + i as u64, Ordering::SeqCst);
        f.create(json!({ "url": "https://a.example", "code": code }))
            .await;
    }

    let page = |q: &'static str| {
        let server = f.server;
        async move {
            server
                .get(&format!("/api/links{q}"))
                .header("x-api-key", KEY)
                .send()
                .await
                .json()
        }
    };
    let first = page("?limit=2").await;
    assert_eq!(first["total"], 5);
    let codes = |v: &serde_json::Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["code"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(codes(&first), ["eee", "ddd"]);
    assert_eq!(codes(&page("?limit=2&offset=2").await), ["ccc", "bbb"]);
    assert_eq!(codes(&page("?limit=2&offset=4").await), ["aaa"]);
    assert_eq!(page("?limit=1000").await["limit"], 100);
    let bad = f
        .server
        .get("/api/links?limit=abc")
        .header("x-api-key", KEY)
        .send()
        .await;
    assert_eq!(bad.status, 400);

    let del = f
        .server
        .delete("/api/links/ccc")
        .header("x-api-key", KEY)
        .send()
        .await;
    assert_eq!(del.status, 204);
    assert_eq!(f.server.get("/ccc").send().await.status, 404);
    let again = f
        .server
        .delete("/api/links/ccc")
        .header("x-api-key", KEY)
        .send()
        .await;
    assert_eq!(again.status, 404);
    assert_eq!(page("").await["total"], 4);
}

#[tokio::test]
async fn links_survive_a_restart_via_the_data_file() {
    let file = std::env::temp_dir().join(format!("ullun-shortener-{}.json", std::process::id()));
    std::fs::remove_file(&file).ok();
    let clock = Arc::new(AtomicU64::new(10));
    let make = |clock: Arc<AtomicU64>| {
        let mut c = config(clock, 1000);
        c.data_file = Some(file.clone());
        c
    };

    let first = spawn(shortener::build(make(clock.clone()))).await;
    let created = first
        .post("/api/links")
        .header("x-api-key", KEY)
        .json(&json!({ "url": "https://persist.example", "code": "kept" }))
        .send()
        .await;
    assert_eq!(created.status, 201);
    let gone = first
        .post("/api/links")
        .header("x-api-key", KEY)
        .json(&json!({ "url": "https://x.example", "code": "removed" }))
        .send()
        .await;
    assert_eq!(gone.status, 201);
    first
        .delete("/api/links/removed")
        .header("x-api-key", KEY)
        .send()
        .await;

    // "restart": a new app over the same file
    let second = spawn(shortener::build(make(clock))).await;
    let hop = second.get("/kept").send().await;
    assert_eq!(
        (hop.status, hop.header("location")),
        (302, "https://persist.example")
    );
    assert_eq!(second.get("/removed").send().await.status, 404);
    // no stray temp file left behind
    assert!(!file.with_extension("tmp").exists());

    // a corrupt file must not stop the service from starting
    std::fs::write(&file, b"{ not json").unwrap();
    let third = spawn(shortener::build(make(Arc::new(AtomicU64::new(10))))).await;
    assert_eq!(third.get("/healthz").send().await.status, 200);
    std::fs::remove_file(&file).ok();
}

#[tokio::test]
async fn openapi_document_describes_the_api() {
    let f = fixture().await;
    let doc = f.server.get("/openapi.json").send().await.json();
    assert_eq!(doc["info"]["title"], "URL shortener");
    for path in ["/api/links", "/api/links/{code}", "/{code}"] {
        assert!(doc["paths"][path].is_object(), "{path}");
    }
    assert_eq!(
        doc["paths"]["/api/links"]["post"]["security"],
        json!([{ "apiKey_x_api_key": [] }])
    );
    assert_eq!(
        doc["components"]["securitySchemes"]["apiKey_x_api_key"]["name"],
        "x-api-key"
    );
    assert!(doc["paths"]["/{code}"]["get"]["responses"]["410"].is_object());
}

#[tokio::test]
async fn oversized_bodies_are_rejected() {
    let f = fixture().await;
    let res = f
        .server
        .post("/api/links")
        .header("x-api-key", KEY)
        .header("content-type", "application/json")
        .body(vec![b'x'; 9 * 1024])
        .send()
        .await;
    assert_eq!(res.status, 413);
}
