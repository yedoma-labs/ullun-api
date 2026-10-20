//! Checkout API for a browser shop
//!
//! What a payment-style endpoint needs beyond routing: the browser app lives on
//! another origin (CORS), a double click or a flaky network must not place two
//! orders (idempotency keys), stock must never go negative, and an overloaded
//! server should shed load instead of falling over.
//!
//! Configuration (environment):
//!   PORT            listen port (default 3000)
//!   SHOP_ORIGIN     origin of the web shop (default http://localhost:5173)
//!   TOKENS          comma separated `token:customer` pairs (default demo-token:alice)
//!
//! Run:   cargo run --example orders
//! Try:   curl -s localhost:3000/api/products
//!        curl -s -X POST localhost:3000/api/orders -H 'authorization: Bearer demo-token' \
//!             -H 'idempotency-key: 7d1c0c4e' -H 'content-type: application/json' \
//!             -d '{"items":[{"sku":"mug","quantity":2}]}'
//!        (repeat the same command: same order comes back, `idempotency-replayed: true`)

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    app::build(app::Config::from_env())
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
