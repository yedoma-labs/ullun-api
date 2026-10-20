//! Idempotency keys over real HTTP

mod common;

use common::spawn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ullun::idempotency::Idempotency;
use ullun::prelude::*;

struct Counter(Arc<AtomicUsize>);

fn app(idem: Idempotency) -> (App, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = App::new()
        .state(Counter(calls.clone()))
        .middleware(idem)
        .post("/pay", |req: Request| async move {
            let n = req.state::<Counter>()?.0.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Response::json(serde_json::json!({ "charge": n }))
                .with_status(http::StatusCode::CREATED)
                .with_header("x-receipt", format!("r{n}")))
        })
        .post("/slow", |req: Request| async move {
            req.state::<Counter>()?.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(Response::text("done"))
        })
        .post("/flaky", |req: Request| async move {
            let n = req.state::<Counter>()?.0.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                Err(Error::internal("first try fails"))
            } else {
                Ok(Response::text("ok"))
            }
        })
        .get("/read", |_req: Request| async { Ok(Response::text("r")) });
    (app, calls)
}

#[tokio::test]
async fn retry_replays_the_stored_response_without_running_the_handler() {
    let (app, calls) = app(Idempotency::new());
    let s = spawn(app).await;

    let first = s
        .post("/pay")
        .header("idempotency-key", "k1")
        .text("a")
        .send()
        .await;
    assert_eq!(first.status, 201);
    assert!(!first.headers.contains_key("idempotency-replayed"));

    let again = s
        .post("/pay")
        .header("idempotency-key", "k1")
        .text("a")
        .send()
        .await;
    assert_eq!(again.status, 201);
    assert_eq!(again.body, first.body);
    assert_eq!(again.header("x-receipt"), "r1");
    assert_eq!(again.header("idempotency-replayed"), "true");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // a new key is a new operation
    let other = s
        .post("/pay")
        .header("idempotency-key", "k2")
        .text("a")
        .send()
        .await;
    assert_eq!(other.header("x-receipt"), "r2");
}

#[tokio::test]
async fn same_key_with_a_different_payload_is_rejected() {
    let (app, calls) = app(Idempotency::new());
    let s = spawn(app).await;
    s.post("/pay")
        .header("idempotency-key", "k")
        .text("100")
        .send()
        .await;
    let other_body = s
        .post("/pay")
        .header("idempotency-key", "k")
        .text("999")
        .send()
        .await;
    assert_eq!(other_body.status, 422);
    let other_query = s
        .post("/pay?x=1")
        .header("idempotency-key", "k")
        .text("100")
        .send()
        .await;
    assert_eq!(other_query.status, 422);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_duplicate_gets_409_and_can_retry_afterwards() {
    let (app, calls) = app(Idempotency::new());
    let s = spawn(app).await;

    let first = tokio::spawn(async move {
        s.post("/slow")
            .header("idempotency-key", "k")
            .text("x")
            .send()
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let dup = s
        .post("/slow")
        .header("idempotency-key", "k")
        .text("x")
        .send()
        .await;
    assert_eq!(dup.status, 409);
    assert_eq!(dup.header("retry-after"), "1");

    assert_eq!(first.await.unwrap().status, 200);
    let retry = s
        .post("/slow")
        .header("idempotency-key", "k")
        .text("x")
        .send()
        .await;
    assert_eq!(retry.header("idempotency-replayed"), "true");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn server_errors_are_not_stored_so_the_retry_runs_again() {
    let (app, calls) = app(Idempotency::new());
    let s = spawn(app).await;
    let first = s.post("/flaky").header("idempotency-key", "k").send().await;
    assert_eq!(first.status, 500);
    tokio::time::sleep(Duration::from_millis(50)).await; // claim release is async
    let retry = s.post("/flaky").header("idempotency-key", "k").send().await;
    assert_eq!(retry.status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn keys_are_scoped_to_the_caller() {
    let (app, calls) = app(Idempotency::new());
    let s = spawn(app).await;
    let alice = s
        .post("/pay")
        .header("idempotency-key", "k")
        .bearer("alice")
        .send()
        .await;
    let bob = s
        .post("/pay")
        .header("idempotency-key", "k")
        .bearer("bob")
        .send()
        .await;
    assert_ne!(alice.header("x-receipt"), bob.header("x-receipt"));
    assert!(!bob.headers.contains_key("idempotency-replayed"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let alice_again = s
        .post("/pay")
        .header("idempotency-key", "k")
        .bearer("alice")
        .send()
        .await;
    assert_eq!(alice_again.header("x-receipt"), "r1");
}

#[tokio::test]
async fn custom_scope_function_is_used() {
    let (app, calls) = app(Idempotency::new().scope_by(|_| "everyone".into()));
    let s = spawn(app).await;
    s.post("/pay")
        .header("idempotency-key", "k")
        .bearer("alice")
        .send()
        .await;
    let bob = s
        .post("/pay")
        .header("idempotency-key", "k")
        .bearer("alice")
        .send()
        .await;
    assert_eq!(bob.header("idempotency-replayed"), "true");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_key_passes_through_unless_required() {
    let (app1, calls1) = app(Idempotency::new());
    let s = spawn(app1).await;
    s.post("/pay").send().await;
    s.post("/pay").send().await;
    assert_eq!(calls1.load(Ordering::SeqCst), 2);

    let (app2, calls2) = app(Idempotency::new().required());
    let s = spawn(app2).await;
    assert_eq!(s.post("/pay").send().await.status, 400);
    assert_eq!(calls2.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_keys_are_rejected_and_other_methods_ignored() {
    let (app, _) = app(Idempotency::new());
    let s = spawn(app).await;
    assert_eq!(
        s.post("/pay")
            .header("idempotency-key", "has space")
            .send()
            .await
            .status,
        400
    );
    let long = "x".repeat(300);
    assert_eq!(
        s.post("/pay")
            .header("idempotency-key", &long)
            .send()
            .await
            .status,
        400
    );
    // GET is not subject to the middleware
    let r = s
        .get("/read")
        .header("idempotency-key", "has space")
        .send()
        .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn lapsed_claim_does_not_block_forever() {
    let (app, calls) = app(Idempotency::new().lock_ttl(Duration::from_millis(100)));
    let s = spawn(app).await;
    let slow = tokio::spawn(async move {
        s.post("/slow")
            .header("idempotency-key", "k")
            .text("x")
            .send()
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    // the first request is still running but its claim lapsed
    let second = s
        .post("/slow")
        .header("idempotency-key", "k")
        .text("x")
        .send()
        .await;
    assert_eq!(second.status, 200);
    slow.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
