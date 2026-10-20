//! ETag middleware and If-Match preconditions over real HTTP

mod common;

use common::spawn;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use ullun::conditional::{etag_for, require_if_match, ETags};
use ullun::prelude::*;

#[tokio::test]
async fn etags_middleware_tags_and_answers_304() {
    let server = spawn(
        App::new()
            .middleware(ETags)
            .get("/doc", |_req: Request| async {
                Ok(Response::json(serde_json::json!({ "v": 1 }))
                    .with_header("cache-control", "max-age=60"))
            })
            .get("/own-tag", |_req: Request| async {
                Ok(Response::text("body").with_header("etag", "\"mine\""))
            })
            .get("/empty", |_req: Request| async { Ok(Response::ok()) })
            .post("/doc", |_req: Request| async {
                Ok(Response::text("posted"))
            })
            .get("/missing-body", |_req: Request| async {
                Err::<Response, _>(Error::not_found("nope"))
            }),
    )
    .await;

    let first = server.get("/doc").send().await;
    assert_eq!(first.status, 200);
    let tag = first.header("etag").to_string();
    assert_eq!(tag, etag_for(&first.body));

    let same = server
        .get("/doc")
        .header("if-none-match", &tag)
        .send()
        .await;
    assert_eq!(same.status, 304);
    assert!(same.body.is_empty());
    assert_eq!(same.header("etag"), tag);
    assert_eq!(same.header("cache-control"), "max-age=60");
    assert_eq!(same.header("content-type"), "");

    let stale = server
        .get("/doc")
        .header("if-none-match", "\"old\"")
        .send()
        .await;
    assert_eq!(stale.status, 200);
    assert_eq!(
        server
            .get("/doc")
            .header("if-none-match", "*")
            .send()
            .await
            .status,
        304
    );
    assert_eq!(
        server
            .get("/doc")
            .header("if-none-match", &format!("W/{tag}"))
            .send()
            .await
            .status,
        304
    );

    // existing tags are respected; other methods and errors are left alone
    let own = server
        .get("/own-tag")
        .header("if-none-match", "\"mine\"")
        .send()
        .await;
    assert_eq!((own.status, own.header("etag")), (304, "\"mine\""));
    let posted = server
        .post("/doc")
        .header("if-none-match", "*")
        .send()
        .await;
    assert_eq!((posted.status, posted.header("etag")), (200, ""));
    let err = server
        .get("/missing-body")
        .header("if-none-match", "*")
        .send()
        .await;
    assert_eq!(err.status, 404);
    assert_eq!(
        server.get("/empty").send().await.header("etag"),
        etag_for(b"")
    );
}

#[tokio::test]
async fn if_match_guards_updates_against_lost_writes() {
    let version = Arc::new(AtomicU64::new(1));
    let (v_get, v_put) = (version.clone(), version.clone());
    let tag = |v: u64| format!("\"v{v}\"");
    let server = spawn(
        App::new()
            .get("/item", move |_req: Request| {
                let v = v_get.clone();
                async move {
                    Ok(Response::text("item").with_header("etag", tag(v.load(Ordering::SeqCst))))
                }
            })
            .put("/item", move |req: Request| {
                let v = v_put.clone();
                async move {
                    require_if_match(&req, &tag(v.load(Ordering::SeqCst)))?;
                    v.fetch_add(1, Ordering::SeqCst);
                    Ok(Response::text("updated"))
                }
            }),
    )
    .await;

    let seen = server.get("/item").send().await.header("etag").to_string();
    assert_eq!(seen, "\"v1\"");

    assert_eq!(server.put("/item").send().await.status, 428);
    assert_eq!(
        server
            .put("/item")
            .header("if-match", "\"v0\"")
            .send()
            .await
            .status,
        412
    );
    assert_eq!(
        server
            .put("/item")
            .header("if-match", "W/\"v1\"")
            .send()
            .await
            .status,
        412
    );
    assert_eq!(
        server
            .put("/item")
            .header("if-match", &seen)
            .send()
            .await
            .status,
        200
    );
    // the same (now stale) tag cannot be used twice
    assert_eq!(
        server
            .put("/item")
            .header("if-match", &seen)
            .send()
            .await
            .status,
        412
    );
    assert_eq!(server.get("/item").send().await.header("etag"), "\"v2\"");
    assert_eq!(
        server
            .put("/item")
            .header("if-match", "*")
            .send()
            .await
            .status,
        200
    );
}
