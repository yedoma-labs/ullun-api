//! The checkout API example, tested over real HTTP

mod common;

// from_env is only used by the example binary
#[allow(dead_code)]
#[path = "../examples/orders/app.rs"]
mod orders;

use common::{spawn, Server};
use serde_json::json;
use std::time::Duration;

const SHOP: &str = "https://shop.example.com";

async fn server() -> Server {
    server_with(256, Duration::from_secs(10)).await
}

async fn server_with(max_in_flight: usize, request_timeout: Duration) -> Server {
    spawn(orders::build(orders::Config {
        shop_origin: SHOP.into(),
        tokens: vec![
            ("tok-a".into(), "alice".into()),
            ("tok-b".into(), "bob".into()),
        ],
        max_in_flight,
        request_timeout,
    }))
    .await
}

fn order(sku: &str, quantity: u32) -> serde_json::Value {
    json!({ "items": [{ "sku": sku, "quantity": quantity }] })
}

#[tokio::test]
async fn a_retried_order_is_placed_once() {
    let s = server().await;
    let place = || {
        s.post("/api/orders")
            .bearer("tok-a")
            .header("idempotency-key", "abc-123")
            .json(&order("mug", 2))
            .send()
    };
    let first = place().await;
    assert_eq!(first.status, 201);
    assert_eq!(first.json()["total_cents"], 2400);
    let again = place().await;
    assert_eq!(again.status, 201);
    assert_eq!(again.header("idempotency-replayed"), "true");
    assert_eq!(again.json()["id"], first.json()["id"]);

    // stock was taken once
    let products = s.get("/api/products").send().await.json();
    let mug = products
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["sku"] == "mug")
        .unwrap();
    assert_eq!(mug["in_stock"], 3);
}

#[tokio::test]
async fn a_key_is_required_and_orders_need_a_token() {
    let s = server().await;
    let no_key = s
        .post("/api/orders")
        .bearer("tok-a")
        .json(&order("mug", 1))
        .send()
        .await;
    assert_eq!(no_key.status, 400);
    let no_token = s
        .post("/api/orders")
        .header("idempotency-key", "k")
        .json(&order("mug", 1))
        .send()
        .await;
    assert_eq!(no_token.status, 401);
}

#[tokio::test]
async fn stock_never_goes_negative_even_with_new_keys() {
    let s = server().await;
    for (key, expected) in [("k1", 201), ("k2", 409)] {
        let r = s
            .post("/api/orders")
            .bearer("tok-a")
            .header("idempotency-key", key)
            .json(&order("shirt", 2))
            .send()
            .await;
        assert_eq!(r.status, expected, "{key}");
    }
    // the refused order is stored too: retrying it does not suddenly succeed
    let retry = s
        .post("/api/orders")
        .bearer("tok-a")
        .header("idempotency-key", "k2")
        .json(&order("shirt", 2))
        .send()
        .await;
    assert_eq!(retry.status, 409);
}

#[tokio::test]
async fn validation_reports_every_problem() {
    let s = server().await;
    let r = s
        .post("/api/orders")
        .bearer("tok-a")
        .header("idempotency-key", "k")
        .json(&json!({ "items": [{ "sku": "nope", "quantity": 0 }] }))
        .send()
        .await;
    assert_eq!(r.status, 422);
    assert!(r.json()["fields"]["items[0]"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn customers_only_see_their_own_orders() {
    let s = server().await;
    let placed = s
        .post("/api/orders")
        .bearer("tok-a")
        .header("idempotency-key", "k")
        .json(&order("sticker", 1))
        .send()
        .await;
    let location = placed.header("location").to_string();
    assert_eq!(s.get(&location).bearer("tok-a").send().await.status, 200);
    assert_eq!(s.get(&location).bearer("tok-b").send().await.status, 404);
}

#[tokio::test]
async fn the_shop_origin_may_call_with_credentials_others_may_not() {
    let s = server().await;
    let ok = s.get("/api/products").header("origin", SHOP).send().await;
    assert_eq!(ok.header("access-control-allow-origin"), SHOP);
    assert_eq!(ok.header("access-control-allow-credentials"), "true");
    let evil = s
        .get("/api/products")
        .header("origin", "https://evil.example")
        .send()
        .await;
    assert!(!evil.headers.contains_key("access-control-allow-origin"));

    let preflight = s
        .request("OPTIONS", "/api/orders")
        .header("origin", SHOP)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization, idempotency-key, content-type",
        )
        .send()
        .await;
    assert_eq!(preflight.status, 204);
    assert!(preflight
        .header("access-control-allow-headers")
        .contains("idempotency-key"));
}

#[tokio::test]
async fn responses_carry_security_headers_and_oversized_bodies_are_refused() {
    let s = server().await;
    let r = s.get("/api/products").send().await;
    assert_eq!(r.header("x-content-type-options"), "nosniff");
    assert!(r
        .header("content-security-policy")
        .contains("default-src 'none'"));

    let big = s
        .post("/api/orders")
        .bearer("tok-a")
        .header("idempotency-key", "big")
        .body(vec![b'x'; 64 * 1024])
        .send()
        .await;
    assert_eq!(big.status, 413);
}
