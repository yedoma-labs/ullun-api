//! Scenario: webhook receiver (payment / git / SaaS callbacks)
//!
//! - shared-secret check with constant-time comparison
//! - small body limit, strict JSON parsing
//! - idempotency: redelivered events (same `id`) are acknowledged, not reprocessed
//!   (the in-memory id set grows unbounded: use a TTL cache or the database)
//! - acknowledge fast with `202`, process on a background task via a channel
//!
//! For real providers verify their HMAC signature over `req.body` (e.g. with
//! the `hmac` + `sha2` crates) instead of the static token used here.
//!
//! Port: set PORT (default 3000)
//! Run:  cargo run --example webhook_receiver
//! Try:  curl -i -X POST localhost:3000/webhooks/payments \
//!         -H 'x-webhook-token: s3cret' -H 'content-type: application/json' \
//!         -d '{"id":"evt_1","type":"payment.succeeded","amount":4200}'

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use ullun::prelude::*;

const SECRET: &str = "s3cret";

#[derive(Debug, Deserialize)]
struct Event {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    amount: u64,
}

/// Compare without early exit so response timing does not leak the secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

#[tokio::main]
async fn main() {
    let (tx, mut rx) = mpsc::channel::<Event>(1024);
    let seen: Arc<Mutex<HashSet<String>>> = Arc::default();

    // worker: slow business logic stays off the request path
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            println!(
                "processing {} ({}) amount={}",
                event.id, event.kind, event.amount
            );
        }
    });

    let receive = move |req: Request| {
        let tx = tx.clone();
        let seen = seen.clone();
        async move {
            let token = req
                .headers
                .get("x-webhook-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !constant_time_eq(token.as_bytes(), SECRET.as_bytes()) {
                return Err(Error::unauthorized("invalid webhook token"));
            }

            let event: Event = req.json()?;
            let first_delivery = seen
                .lock()
                .expect("dedupe lock poisoned")
                .insert(event.id.clone());
            if !first_delivery {
                return Ok(Response::json(serde_json::json!({ "status": "duplicate" })));
            }

            // queue full -> 503 so the sender retries later
            let id = event.id.clone();
            if tx.try_send(event).is_err() {
                // forget the id so the provider's retry is processed, not skipped
                seen.lock().expect("dedupe lock poisoned").remove(&id);
                return Err(Error::custom(503, "queue full, retry later"));
            }
            let mut res = Response::json(serde_json::json!({ "status": "queued" }));
            res.status = http::StatusCode::ACCEPTED;
            Ok(res)
        }
    };

    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );

    App::new()
        .max_body_size(Some(64 * 1024)) // webhooks are small; reject the rest with 413
        .post("/webhooks/payments", receive)
        .get("/healthz", |_req: Request| async {
            Ok(Response::text("ok"))
        })
        .run(&addr)
        .await
        .unwrap();
}
