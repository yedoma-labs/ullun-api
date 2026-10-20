//! Outgoing webhooks as a service
//!
//! Customers register HTTP endpoints; your systems publish events; this service
//! delivers every event to each subscribed endpoint with a signed request,
//! retrying failures with exponential backoff, honouring `Retry-After`, parking
//! hopeless deliveries in a dead-letter list for the operator, disabling
//! endpoints that keep failing, and surviving restarts (queue and endpoints are
//! saved to disk). Targets are vetted against SSRF at registration and again at
//! every delivery.
//!
//! Configuration (environment):
//!   PORT                  listen port (default 3000)
//!   WEBHOOKS_API_KEY      key for the management API (required unless DEV=1)
//!   DATA_DIR              where endpoints and the queue are saved (default ./webhooks-data)
//!   ALLOW_PRIVATE_TARGETS 1 to deliver to loopback/private addresses
//!   DEV                   1 = demo key, private targets allowed
//!
//! Run:   DEV=1 cargo run --example webhooks
//! Try:   curl -s -X POST localhost:3000/api/endpoints -H 'x-api-key: dev-webhooks-key-dev-key' \
//!             -H 'content-type: application/json' -d '{"url":"http://127.0.0.1:9000/hook","events":["order.created"]}'
//!        curl -s -X POST localhost:3000/api/events -H 'x-api-key: dev-webhooks-key-dev-key' \
//!             -H 'idempotency-key: order-17-created' -H 'content-type: application/json' \
//!             -d '{"type":"order.created","data":{"order_id":17}}'
//!
//! Receivers verify `X-Webhook-Signature` with `app::verify_signature`.

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
    app::build(config)
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
