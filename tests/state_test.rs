//! Shared application state

mod common;

use common::spawn;
use std::sync::atomic::{AtomicU64, Ordering};
use ullun::prelude::*;

#[derive(Default)]
struct Hits(AtomicU64);

struct Greeting(&'static str);

async fn hit(req: Request) -> Result<Response> {
    let n = req.state::<Hits>()?.0.fetch_add(1, Ordering::Relaxed) + 1;
    let greeting = req.state::<Greeting>()?.0;
    Ok(Response::text(format!("{greeting} #{n}")))
}

#[tokio::test]
async fn state_is_shared_across_requests_and_connections() {
    let server = spawn(
        App::new()
            .state(Hits::default())
            .state(Greeting("hello"))
            .get("/", hit),
    )
    .await;

    assert_eq!(server.get("/").send().await.text(), "hello #1");
    assert_eq!(server.get("/").send().await.text(), "hello #2");
}

#[tokio::test]
async fn state_is_visible_in_middleware_and_replaced_by_type() {
    let server = spawn(
        App::new()
            .state(Greeting("first"))
            .state(Greeting("second"))
            .middleware(|req: Request, next: Next| async move {
                let tag = req.try_state::<Greeting>().map(|g| g.0).unwrap_or("none");
                let res = next.run(req).await?;
                Ok(res.with_header("x-greeting", tag))
            })
            .get("/", |_req: Request| async { Ok(Response::text("ok")) }),
    )
    .await;

    let res = server.get("/").send().await;
    assert_eq!(res.header("x-greeting"), "second");
}

#[tokio::test]
async fn missing_state_is_a_500_not_a_panic() {
    let server = spawn(App::new().get("/", hit)).await;
    let res = server.get("/").send().await;
    assert_eq!(res.status, 500);
    assert!(res.text().contains("not registered"));
}

#[tokio::test]
async fn state_arc_outlives_the_request() {
    let server = spawn(
        App::new()
            .state(Hits::default())
            .middleware(|req: Request, next: Next| async move {
                // `req` is moved into next.run, so a borrowed &Hits would not do
                let hits = req.state_arc::<Hits>()?;
                let res = next.run(req).await?;
                Ok(res.with_header("x-hits-after", hits.0.load(Ordering::Relaxed).to_string()))
            })
            .get("/", |req: Request| async move {
                let hits = req.state_arc::<Hits>()?;
                tokio::spawn(async move {
                    hits.0.fetch_add(1, Ordering::Relaxed);
                });
                Ok(Response::text("ok"))
            }),
    )
    .await;
    assert_eq!(server.get("/").send().await.status, 200);
    assert!(server
        .get("/")
        .send()
        .await
        .header("x-hits-after")
        .parse::<u32>()
        .is_ok());
    let missing = spawn(App::new().get("/", |req: Request| async move {
        req.state_arc::<Greeting>()?;
        Ok(Response::text("unreachable"))
    }))
    .await;
    assert_eq!(missing.get("/").send().await.status, 500);
}
