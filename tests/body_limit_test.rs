//! Per-route request body limits

mod common;

use common::spawn;
use ullun::prelude::*;

fn echo_len() -> impl Fn(Request) -> std::future::Ready<Result<Response>> + Send + Sync + 'static {
    |req: Request| std::future::ready(Ok(Response::text(req.body.len().to_string())))
}

#[tokio::test]
async fn longest_prefix_wins_over_the_default() {
    let s = spawn(
        App::new()
            .max_body_size(Some(100))
            .body_limit("/upload", Some(1000))
            .body_limit("/upload/tiny", Some(10))
            .body_limit("/bulk/", None)
            .post("/small", echo_len())
            .post("/upload", echo_len())
            .post("/upload/big", echo_len())
            .post("/upload/tiny", echo_len())
            .post("/uploads", echo_len())
            .post("/bulk/import", echo_len()),
    )
    .await;

    let post = |path: &str, n: usize| {
        let call = s.post(path).body(vec![b'x'; n]);
        async move { call.send().await.status }
    };
    // default
    assert_eq!(post("/small", 100).await, 200);
    assert_eq!(post("/small", 101).await, 413);
    // a rule for a prefix applies to the prefix itself and below it ...
    assert_eq!(post("/upload", 1000).await, 200);
    assert_eq!(post("/upload", 1001).await, 413);
    assert_eq!(post("/upload/big", 900).await, 200);
    // ... the longest rule wins ...
    assert_eq!(post("/upload/tiny", 10).await, 200);
    assert_eq!(post("/upload/tiny", 11).await, 413);
    // ... and "/upload" is not a prefix of "/uploads"
    assert_eq!(post("/uploads", 101).await, 413);
    // None lifts the limit
    assert_eq!(post("/bulk/import", 5000).await, 200);
}

#[tokio::test]
async fn groups_can_set_their_own_limit() {
    let s = spawn(
        App::new()
            .max_body_size(Some(50))
            .post("/api/note", echo_len())
            .group("/files", |g| {
                g.max_body_size(Some(500)).post("/up", echo_len())
            }),
    )
    .await;

    assert_eq!(
        s.post("/api/note").body(vec![b'x'; 51]).send().await.status,
        413
    );
    assert_eq!(
        s.post("/files/up")
            .body(vec![b'x'; 500])
            .send()
            .await
            .status,
        200
    );
    assert_eq!(
        s.post("/files/up")
            .body(vec![b'x'; 501])
            .send()
            .await
            .status,
        413
    );
}

#[tokio::test]
async fn limits_are_enforced_per_route_on_the_declared_length() {
    let s = spawn(
        App::new()
            .max_body_size(Some(10))
            .body_limit("/big", Some(200))
            .post("/big", echo_len())
            .post("/small", echo_len()),
    )
    .await;
    assert_eq!(
        s.post("/big").body(vec![b'x'; 200]).send().await.status,
        200
    );
    assert_eq!(
        s.post("/big").body(vec![b'x'; 201]).send().await.status,
        413
    );
    assert_eq!(
        s.post("/small").body(vec![b'x'; 11]).send().await.status,
        413
    );
}

#[tokio::test]
async fn oversized_uploads_get_a_readable_413_not_a_connection_reset() {
    // the server drains what the client is still sending before answering,
    // otherwise the OS resets the connection and the response is lost
    let s = spawn(App::new().max_body_size(Some(1024)).post("/up", echo_len())).await;
    for size in [2 * 1024, 300 * 1024, 3 * 1024 * 1024] {
        let res = s.post("/up").body(vec![b'x'; size]).send().await;
        assert_eq!(res.status, 413, "{size} bytes");
        assert_eq!(res.header("connection"), "close");
        assert!(res.text().contains("too large"));
    }
    // the server is fine afterwards
    assert_eq!(
        s.post("/up").body(vec![b'x'; 1024]).send().await.status,
        200
    );
}
