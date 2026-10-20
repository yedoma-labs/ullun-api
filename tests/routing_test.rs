//! HEAD, OPTIONS and 405 handling

mod common;

use common::spawn;
use ullun::conditional::ETags;
use ullun::prelude::*;

fn users_app() -> App {
    App::new()
        .get("/users/{id}", |req: Request| async move {
            Ok(Response::json(
                serde_json::json!({ "id": req.params.get("id")? }),
            ))
        })
        .delete("/users/{id}", |_req: Request| async {
            Ok(Response::new(http::StatusCode::NO_CONTENT))
        })
        .post("/users", |_req: Request| async {
            Ok(Response::text("created"))
        })
        .get("/ping", |_req: Request| async {
            Ok(Response::text("pong"))
        })
}

#[tokio::test]
async fn head_is_answered_by_the_get_handler_without_a_body() {
    let s = spawn(users_app()).await;

    let get = s.get("/users/42").send().await;
    let head = s.request("HEAD", "/users/42").send().await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-type"), "application/json");
    // the length the GET would have had
    assert_eq!(head.header("content-length"), get.body.len().to_string());

    assert_eq!(
        s.request("HEAD", "/ping")
            .send()
            .await
            .header("content-length"),
        "4"
    );
    // unknown path: 404, and no GET route for HEAD on a POST-only path: 405
    assert_eq!(s.request("HEAD", "/missing").send().await.status, 404);
    assert_eq!(s.request("HEAD", "/users").send().await.status, 405);
}

#[tokio::test]
async fn head_works_with_middleware_that_inspects_the_body() {
    let s = spawn(users_app().middleware(ETags)).await;
    let get = s.get("/ping").send().await;
    let head = s.request("HEAD", "/ping").send().await;
    assert_eq!(head.header("etag"), get.header("etag"));
    assert!(head.body.is_empty());

    let cached = s
        .request("HEAD", "/ping")
        .header("if-none-match", get.header("etag"))
        .send()
        .await;
    assert_eq!(cached.status, 304);
    assert_eq!(cached.header("content-length"), "");
}

#[tokio::test]
async fn wrong_methods_get_405_with_an_allow_header() {
    let s = spawn(users_app()).await;

    let res = s.put("/users/42").send().await;
    assert_eq!(res.status, 405);
    assert_eq!(res.header("allow"), "GET, HEAD, DELETE, OPTIONS");
    assert_eq!(res.json()["status"], 405);

    assert_eq!(
        s.get("/users").send().await.header("allow"),
        "POST, OPTIONS"
    );
    assert_eq!(
        s.patch("/ping").send().await.header("allow"),
        "GET, HEAD, OPTIONS"
    );
    // methods the router does not know at all are 405 on known paths, 404 otherwise
    assert_eq!(s.request("TRACE", "/ping").send().await.status, 405);
    assert_eq!(s.request("TRACE", "/missing").send().await.status, 404);
    assert_eq!(s.put("/missing").send().await.status, 404);
}

#[tokio::test]
async fn options_lists_the_supported_methods() {
    let s = spawn(users_app()).await;
    let res = s.request("OPTIONS", "/users/42").send().await;
    assert_eq!(res.status, 204);
    assert_eq!(res.header("allow"), "GET, HEAD, DELETE, OPTIONS");
    assert!(res.body.is_empty());
    assert_eq!(s.request("OPTIONS", "/missing").send().await.status, 404);
}

#[tokio::test]
async fn middleware_still_wraps_405_and_cors_preflight_wins_over_automatic_options() {
    let s = spawn(
        users_app()
            .middleware(ullun::cors::Cors::new().allow_origin("https://app.example.com"))
            .middleware(|req: Request, next: Next| async move {
                let res = next.run(req).await?;
                Ok(res.with_header("x-seen", "1"))
            }),
    )
    .await;

    let wrong = s
        .put("/ping")
        .header("origin", "https://app.example.com")
        .send()
        .await;
    assert_eq!((wrong.status, wrong.header("x-seen")), (405, "1"));
    assert_eq!(
        wrong.header("access-control-allow-origin"),
        "https://app.example.com"
    );

    let preflight = s
        .request("OPTIONS", "/ping")
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "DELETE")
        .send()
        .await;
    assert_eq!(preflight.status, 204);
    // the CORS answer, not the bare Allow list
    assert!(preflight
        .header("access-control-allow-methods")
        .contains("DELETE"));
    assert_eq!(
        preflight.header("access-control-allow-origin"),
        "https://app.example.com"
    );
}
