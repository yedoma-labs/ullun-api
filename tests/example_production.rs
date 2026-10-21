//! The production skeleton example, tested in-process

// from_env is only used by the example binary
#[allow(dead_code)]
#[path = "../examples/production/app.rs"]
mod production;

use std::sync::{Arc, Mutex};
use std::time::Duration;
use ullun::proxy::TrustedProxies;
use ullun::testing::TestClient;

const SHOP: &str = "https://shop.example.com";

struct Fixture {
    client: TestClient,
    log: Arc<Mutex<Vec<String>>>,
}

fn fixture() -> Fixture {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let client = TestClient::new(production::build(production::Config {
        shop_origin: SHOP.into(),
        admin_key: "admin-secret".into(),
        trusted_proxies: TrustedProxies::new().trust("10.0.0.0/8").unwrap(),
        log_sink: Arc::new(move |l| sink.lock().unwrap().push(l.to_string())),
        shutdown_delay: Duration::from_millis(1),
    }));
    Fixture { client, log }
}

#[tokio::test]
async fn every_response_is_traced_logged_and_hardened() {
    let f = fixture();
    let res = f
        .client
        .get("/api/catalog")
        .header("origin", SHOP)
        .send()
        .await;
    assert_eq!(res.status(), 200);
    assert!(!res.header("x-request-id").is_empty());
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert!(res
        .header("content-security-policy")
        .contains("default-src 'none'"));
    assert_eq!(res.header("access-control-allow-origin"), SHOP);

    let line: serde_json::Value = serde_json::from_str(&f.log.lock().unwrap()[0]).unwrap();
    assert_eq!(line["path"], "/api/catalog");
    assert_eq!(line["route"], "/api/catalog");
    assert_eq!(line["trace_id"].as_str().unwrap().len(), 32);
    assert_eq!(line["request_id"], res.header("x-request-id"));
}

#[tokio::test]
async fn the_catalog_is_cached() {
    let f = fixture();
    assert_eq!(
        f.client.get("/api/catalog").send().await.header("x-cache"),
        "MISS"
    );
    assert_eq!(
        f.client.get("/api/catalog").send().await.header("x-cache"),
        "HIT"
    );
}

#[tokio::test]
async fn orders_need_an_idempotency_key_and_are_placed_once() {
    let f = fixture();
    let post = |key: Option<&'static str>| {
        let mut call = f
            .client
            .post("/api/orders")
            .json(&serde_json::json!({ "sku": "mug" }));
        if let Some(k) = key {
            call = call.header("idempotency-key", k);
        }
        call.send()
    };
    let missing = post(None).await;
    assert_eq!(missing.status(), 400);
    assert_eq!(missing.header("content-type"), "application/problem+json");

    let first = post(Some("abc")).await;
    let again = post(Some("abc")).await;
    assert_eq!(first.status(), 201);
    assert_eq!(again.header("idempotency-replayed"), "true");
    assert_eq!(
        again.json::<serde_json::Value>()["id"],
        first.json::<serde_json::Value>()["id"]
    );
    assert_eq!(
        post(Some("other")).await.json::<serde_json::Value>()["id"],
        2
    );
}

#[tokio::test]
async fn errors_are_problem_json_including_the_fallback() {
    let f = fixture();
    let res = f.client.get("/no/such/thing").send().await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.header("content-type"), "application/problem+json");
    let p: serde_json::Value = res.json();
    assert_eq!(p["type"], "https://api.example.com/problems/not-found");
    assert_eq!(p["detail"], "No such resource");
    // security headers also decorate errors
    assert_eq!(res.header("x-content-type-options"), "nosniff");
}

#[tokio::test]
async fn the_admin_area_needs_the_key_and_names_the_caller() {
    let f = fixture();
    assert_eq!(f.client.get("/admin/stats").send().await.status(), 401);
    assert_eq!(
        f.client
            .get("/admin/stats")
            .header("x-api-key", "wrong")
            .send()
            .await
            .status(),
        401
    );
    let ok = f
        .client
        .get("/admin/stats")
        .header("x-api-key", "admin-secret")
        .send()
        .await;
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.json::<serde_json::Value>()["caller"], "ops");
}

#[tokio::test]
async fn client_addresses_come_from_trusted_proxies_only() {
    let f = fixture();
    let via_lb = f
        .client
        .get("/whoami")
        .remote_addr(([10, 1, 2, 3], 5000))
        .header("x-forwarded-for", "6.6.6.6, 198.51.100.7")
        .header("x-forwarded-proto", "https")
        .send()
        .await
        .json::<serde_json::Value>();
    assert_eq!(via_lb["client_ip"], "198.51.100.7");
    assert_eq!(via_lb["secure"], true);

    let direct = f
        .client
        .get("/whoami")
        .remote_addr(([203, 0, 113, 9], 5000))
        .header("x-forwarded-for", "6.6.6.6")
        .send()
        .await
        .json::<serde_json::Value>();
    assert_eq!(direct["client_ip"], "203.0.113.9");
    assert_eq!(direct["secure"], false);
}

#[tokio::test]
async fn probes_and_metrics_work() {
    let f = fixture();
    assert_eq!(f.client.get("/livez").send().await.status(), 200);
    let ready = f.client.get("/readyz").send().await;
    assert_eq!(ready.status(), 200);
    assert_eq!(ready.json::<serde_json::Value>()["status"], "up");
    f.client.get("/api/catalog").send().await;
    let metrics = f.client.get("/metrics").send().await.text();
    assert!(metrics.contains("http_requests_total"));
    assert!(metrics.contains("route=\"/api/catalog\""));
}

#[tokio::test]
async fn preflights_and_bodies_follow_the_policy() {
    let f = fixture();
    let preflight = f
        .client
        .options("/api/orders")
        .header("origin", SHOP)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "idempotency-key, content-type",
        )
        .send()
        .await;
    assert_eq!(preflight.status(), 204);
    let evil = f
        .client
        .options("/api/orders")
        .header("origin", "https://evil.example")
        .header("access-control-request-method", "POST")
        .send()
        .await;
    assert_eq!(evil.status(), 403);

    let big = f
        .client
        .post("/api/orders")
        .header("idempotency-key", "big")
        .body(vec![b'x'; 100 * 1024])
        .send()
        .await;
    assert_eq!(big.status(), 413);
}
