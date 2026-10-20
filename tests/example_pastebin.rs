//! The pastebin example, tested over real HTTP

mod common;

#[allow(dead_code)]
#[path = "../examples/pastebin/app.rs"]
mod pastebin;

use common::{spawn, Server};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    server: Server,
    clock: Arc<AtomicU64>,
}

async fn fixture_with(max_pastes: usize, limit: u32) -> Fixture {
    let clock = Arc::new(AtomicU64::new(1_000));
    let c = clock.clone();
    let server = spawn(pastebin::build(pastebin::Config {
        base_url: "https://paste.example/".into(),
        max_pastes,
        rate_limit: (limit, Duration::from_secs(60)),
        clock: Arc::new(move || c.load(Ordering::SeqCst)),
    }))
    .await;
    Fixture { server, clock }
}

async fn fixture() -> Fixture {
    fixture_with(1000, 1000).await
}

impl Fixture {
    async fn create(&self, body: serde_json::Value) -> common::Reply {
        self.server.post("/pastes").json(&body).send().await
    }
}

fn id_of(created: &serde_json::Value) -> String {
    created["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn json_api_roundtrip_with_raw_and_html_views() {
    let f = fixture().await;
    let res = f
        .create(json!({ "content": "line 1\nline 2 <b>", "title": "Notes & more" }))
        .await;
    assert_eq!(res.status, 201);
    let created = res.json();
    let id = id_of(&created);
    assert_eq!(id.len(), 10);
    assert_eq!(created["delete_token"].as_str().unwrap().len(), 32);
    assert_eq!(created["url"], format!("https://paste.example/p/{id}"));
    assert_eq!(
        created["raw_url"],
        format!("https://paste.example/raw/{id}")
    );
    assert_eq!(created["expires_at"], 1_000 + 24 * 3600);
    assert_eq!(res.header("location"), created["url"].as_str().unwrap());

    let raw = f.server.get(&format!("/raw/{id}")).send().await;
    assert_eq!(raw.status, 200);
    assert_eq!(raw.text(), "line 1\nline 2 <b>");
    assert_eq!(raw.header("content-type"), "text/plain; charset=utf-8");
    assert_eq!(raw.header("x-content-type-options"), "nosniff");
    assert!(raw.header("content-security-policy").contains("sandbox"));

    let html = f.server.get(&format!("/p/{id}")).send().await;
    assert_eq!(html.status, 200);
    let body = html.text();
    // content and title are escaped, nothing is rendered as markup
    assert!(body.contains("line 2 &lt;b&gt;"), "{body}");
    assert!(body.contains("Notes &amp; more"));
    assert!(!body.contains("<b>"));
    assert!(html
        .header("content-security-policy")
        .contains("default-src 'none'"));
    assert_eq!(html.header("cache-control"), "no-store");
    // no token without the ?token= from the creation redirect
    assert!(!body.contains("Delete token"));
}

#[tokio::test]
async fn plain_text_bodies_work_like_curl_upload() {
    let f = fixture().await;
    let res = f.server.post("/pastes").text("just text").send().await;
    assert_eq!(res.status, 201);
    let id = id_of(&res.json());
    assert_eq!(
        f.server.get(&format!("/raw/{id}")).send().await.text(),
        "just text"
    );

    let other = f
        .server
        .post("/pastes")
        .header("content-type", "application/xml")
        .body(b"<x/>".to_vec())
        .send()
        .await;
    assert_eq!(other.status, 415);
    let bad_utf8 = f
        .server
        .post("/pastes")
        .header("content-type", "text/plain")
        .body(vec![0xff, 0xfe, 0xfd])
        .send()
        .await;
    assert_eq!(bad_utf8.status, 400);
}

#[tokio::test]
async fn validation_of_content_title_and_ttl() {
    let f = fixture().await;
    for (body, status) in [
        (json!({ "content": "" }), 400),
        (json!({ "content": "  \n\t " }), 400),
        (json!({ "content": "x".repeat(64 * 1024 + 1) }), 413),
        (json!({ "content": "x", "title": "t".repeat(81) }), 400),
        (json!({ "content": "x", "title": "bad\ntitle" }), 400),
        (json!({ "content": "x", "ttl_secs": 0 }), 400),
        (
            json!({ "content": "x", "ttl_secs": 7 * 24 * 3600 + 1 }),
            400,
        ),
        (json!({ "title": "no content" }), 400),
    ] {
        let res = f.create(body.clone()).await;
        assert_eq!(
            res.status,
            status,
            "{}",
            body.to_string().chars().take(60).collect::<String>()
        );
    }
    // boundaries are accepted
    assert_eq!(
        f.create(json!({ "content": "x".repeat(64 * 1024) }))
            .await
            .status,
        201
    );
    assert_eq!(
        f.create(json!({ "content": "x", "ttl_secs": 7 * 24 * 3600 }))
            .await
            .status,
        201
    );
    assert_eq!(
        f.create(json!({ "content": "x", "title": "t".repeat(80) }))
            .await
            .status,
        201
    );
    // way over the body limit never reaches the handler
    let huge = f
        .server
        .post("/pastes")
        .text(&"x".repeat(200 * 1024))
        .send()
        .await;
    assert_eq!(huge.status, 413);
}

#[tokio::test]
async fn pastes_expire() {
    let f = fixture().await;
    let id = id_of(
        &f.create(json!({ "content": "short", "ttl_secs": 60 }))
            .await
            .json(),
    );

    f.clock.store(1_059, Ordering::SeqCst);
    assert_eq!(f.server.get(&format!("/raw/{id}")).send().await.status, 200);

    f.clock.store(1_060, Ordering::SeqCst);
    assert_eq!(f.server.get(&format!("/raw/{id}")).send().await.status, 404);
    assert_eq!(f.server.get(&format!("/p/{id}")).send().await.status, 404);
}

#[tokio::test]
async fn burn_after_read_serves_exactly_once() {
    let f = fixture().await;
    let id = id_of(
        &f.create(json!({ "content": "secret", "burn_after_read": true }))
            .await
            .json(),
    );

    assert_eq!(
        f.server.get(&format!("/raw/{id}")).send().await.text(),
        "secret"
    );
    assert_eq!(f.server.get(&format!("/raw/{id}")).send().await.status, 404);
    assert_eq!(f.server.get(&format!("/p/{id}")).send().await.status, 404);

    // the HTML view burns it as well
    let id2 = id_of(
        &f.create(json!({ "content": "again", "burn_after_read": true }))
            .await
            .json(),
    );
    assert_eq!(f.server.get(&format!("/p/{id2}")).send().await.status, 200);
    assert_eq!(
        f.server.get(&format!("/raw/{id2}")).send().await.status,
        404
    );
}

#[tokio::test]
async fn delete_requires_the_token() {
    let f = fixture().await;
    let created = f.create(json!({ "content": "mine" })).await.json();
    let (id, token) = (
        id_of(&created),
        created["delete_token"].as_str().unwrap().to_string(),
    );
    let path = format!("/pastes/{id}");

    assert_eq!(f.server.delete(&path).send().await.status, 403);
    assert_eq!(
        f.server
            .delete(&path)
            .header("x-delete-token", "wrong")
            .send()
            .await
            .status,
        403
    );
    assert_eq!(
        f.server
            .delete(&path)
            .header("x-delete-token", &token[..31])
            .send()
            .await
            .status,
        403
    );
    // the paste survived all of that
    assert_eq!(f.server.get(&format!("/raw/{id}")).send().await.status, 200);

    assert_eq!(
        f.server
            .delete(&path)
            .header("x-delete-token", &token)
            .send()
            .await
            .status,
        204
    );
    assert_eq!(f.server.get(&format!("/raw/{id}")).send().await.status, 404);
    assert_eq!(
        f.server
            .delete(&path)
            .header("x-delete-token", &token)
            .send()
            .await
            .status,
        404
    );
    assert_eq!(f.server.delete("/pastes/unknown").send().await.status, 404);
}

#[tokio::test]
async fn html_form_flow_shows_the_delete_token() {
    let f = fixture().await;

    let page = f.server.get("/").send().await;
    assert_eq!(page.status, 200);
    assert!(page.text().contains(r#"action="/new""#));

    let res = f
        .server
        .post("/new")
        .form("title=Hello+%26+bye&content=a+%3Cscript%3Eb%3C%2Fscript%3E%0Aline2&ttl_hours=1&burn_after_read=")
        .send()
        .await;
    assert_eq!(res.status, 303);
    let location = res.header("location").to_string();
    assert!(
        location.starts_with("/p/") && location.contains("?token="),
        "{location}"
    );

    let (path, token) = location.split_once("?token=").unwrap();
    // the creation redirect shows the token; burn-after-read was ticked, so it is viewable once
    let shown = f.server.get(&location).send().await;
    assert_eq!(shown.status, 200);
    let body = shown.text();
    assert!(body.contains(&format!("<code>{token}</code>")), "{body}");
    assert!(body.contains("a &lt;script&gt;b&lt;/script&gt;\nline2"));
    assert!(body.contains("Hello &amp; bye"));
    assert!(!body.contains("<script>"));
    assert_eq!(f.server.get(path).send().await.status, 404);
}

#[tokio::test]
async fn html_form_errors_rerender_with_status() {
    let f = fixture().await;
    let empty = f.server.post("/new").form("content=++&title=").send().await;
    assert_eq!(empty.status, 400);
    assert!(empty.text().contains("content must not be empty"));
    assert!(empty.text().contains(r#"<form method="post""#));

    let wrong_type = f
        .server
        .post("/new")
        .json(&json!({ "content": "x" }))
        .send()
        .await;
    assert_eq!(wrong_type.status, 415);
    let missing = f.server.post("/new").form("title=only").send().await;
    assert_eq!(missing.status, 400);
}

#[tokio::test]
async fn creation_budget_is_shared_by_api_and_form() {
    let f = fixture_with(1000, 3).await;
    assert_eq!(f.create(json!({ "content": "1" })).await.status, 201);
    assert_eq!(
        f.server.post("/new").form("content=2").send().await.status,
        303
    );
    assert_eq!(f.create(json!({ "content": "3" })).await.status, 201);

    let limited = f.create(json!({ "content": "4" })).await;
    assert_eq!(limited.status, 429);
    assert!(limited.header("retry-after").parse::<u64>().unwrap() >= 1);
    assert_eq!(
        f.server.post("/new").form("content=5").send().await.status,
        429
    );
    // reading is never limited
    for _ in 0..5 {
        assert_eq!(f.server.get("/").send().await.status, 200);
    }
}

#[tokio::test]
async fn storage_cap_answers_507_and_recovers_after_expiry() {
    let f = fixture_with(2, 1000).await;
    assert_eq!(
        f.create(json!({ "content": "a", "ttl_secs": 100 }))
            .await
            .status,
        201
    );
    assert_eq!(
        f.create(json!({ "content": "b", "ttl_secs": 100 }))
            .await
            .status,
        201
    );
    assert_eq!(f.create(json!({ "content": "c" })).await.status, 507);

    f.clock.store(1_100, Ordering::SeqCst);
    assert_eq!(f.create(json!({ "content": "c" })).await.status, 201);
}

#[tokio::test]
async fn unknown_ids_and_health() {
    let f = fixture().await;
    assert_eq!(f.server.get("/raw/nope").send().await.status, 404);
    assert_eq!(f.server.get("/p/nope").send().await.status, 404);
    assert_eq!(f.server.get("/healthz").send().await.json()["status"], "ok");
}
