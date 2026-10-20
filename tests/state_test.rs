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
