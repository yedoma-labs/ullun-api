//! The in-process test client

use serde::{Deserialize, Serialize};
use ullun::prelude::*;
use ullun::testing::TestClient;

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Item {
    name: String,
}

fn app() -> App {
    App::new()
        .state(41u32)
        .get("/hello/{name}", |req: Request| async move {
            Ok(Response::text(format!("hi {}", req.params.get("name")?)))
        })
        .get("/search", |req: Request| async move {
            Ok(Response::text(
                req.query.get("q").unwrap_or("-").to_string(),
            ))
        })
        .post("/items", |req: Request| async move {
            let item: Item = req.json()?;
            Ok(Response::json(&item).with_status(http::StatusCode::CREATED))
        })
        .post("/form", |req: Request| async move {
            let item: Item = req.form()?;
            Ok(Response::text(item.name))
        })
        .get("/state", |req: Request| async move {
            Ok(Response::text((req.state::<u32>()? + 1).to_string()))
        })
        .get("/whoami", |req: Request| async move {
            Ok(Response::text(format!(
                "{} {}",
                req.remote_addr
                    .map(|a| a.ip().to_string())
                    .unwrap_or_default(),
                req.is_secure()
            )))
        })
        .get("/login", |_req: Request| async {
            Ok(Response::text("in").cookie(Cookie::new("sid", "abc")))
        })
        .get("/logout", |_req: Request| async {
            Ok(Response::text("out").cookie(Cookie::new("sid", "").max_age(0)))
        })
        .get("/me", |req: Request| async move {
            Ok(Response::text(
                req.cookies().get("sid").unwrap_or("anonymous").to_string(),
            ))
        })
        .get("/stream", |_req: Request| async {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                for n in 1..=3 {
                    let _ = tx.send(format!("{n},")).await;
                }
            });
            Ok(res)
        })
        .get("/auth", |req: Request| async move {
            Ok(Response::text(
                req.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_string(),
            ))
        })
}

#[tokio::test]
async fn routes_params_query_and_state() {
    let c = TestClient::new(app());
    assert_eq!(c.get("/hello/ada").send().await.text(), "hi ada");
    assert_eq!(c.get("/search?q=rust").send().await.text(), "rust");
    assert_eq!(c.get("/search").send().await.text(), "-");
    assert_eq!(c.get("/state").send().await.text(), "42");
}

#[tokio::test]
async fn json_and_form_bodies() {
    let c = TestClient::new(app());
    let res = c
        .post("/items")
        .json(&Item { name: "x".into() })
        .send()
        .await;
    assert_eq!(res.status(), 201);
    assert_eq!(res.json::<Item>(), Item { name: "x".into() });
    assert!(res.header("content-type").starts_with("application/json"));

    assert_eq!(
        c.post("/form")
            .form(&Item { name: "a b".into() })
            .send()
            .await
            .text(),
        "a b"
    );
    let bad = c.post("/items").text("not json").send().await;
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn misses_methods_and_head_behave_like_the_server() {
    let c = TestClient::new(app());
    assert_eq!(c.get("/nope").send().await.status(), 404);
    let wrong = c.delete("/state").send().await;
    assert_eq!(wrong.status(), 405);
    assert!(wrong.header("allow").contains("GET"));
    let head = c.head("/state").send().await;
    assert_eq!((head.status(), head.bytes().len()), (200, 0));
    assert_eq!(head.header("content-length"), "2");
    assert_eq!(c.options("/state").send().await.status(), 204);
}

#[tokio::test]
async fn peer_address_tls_flag_and_headers() {
    let c = TestClient::new(app());
    assert_eq!(c.get("/whoami").send().await.text(), "127.0.0.1 false");
    let res = c
        .get("/whoami")
        .remote_addr(([203, 0, 113, 9], 4000))
        .https()
        .send()
        .await;
    assert_eq!(res.text(), "203.0.113.9 true");
    assert_eq!(
        c.get("/auth").bearer("tok").send().await.text(),
        "Bearer tok"
    );
}

#[tokio::test]
async fn cookie_jar_follows_set_cookie_and_deletion() {
    let c = TestClient::new(app()).with_cookies();
    assert_eq!(c.get("/me").send().await.text(), "anonymous");
    c.get("/login").send().await;
    assert_eq!(c.cookie("sid").as_deref(), Some("abc"));
    assert_eq!(c.get("/me").send().await.text(), "abc");
    c.get("/logout").send().await;
    assert_eq!(c.cookie("sid"), None);
    assert_eq!(c.get("/me").send().await.text(), "anonymous");

    // without a jar nothing is remembered
    let plain = TestClient::new(app());
    plain.get("/login").send().await;
    assert_eq!(plain.get("/me").send().await.text(), "anonymous");
    assert_eq!(
        plain.get("/me").cookie("sid", "manual").send().await.text(),
        "manual"
    );
    c.get("/login").send().await;
    c.clear_cookies();
    assert_eq!(c.cookie("sid"), None);
}

#[tokio::test]
async fn streaming_bodies_are_collected() {
    let c = TestClient::new(app());
    assert_eq!(c.get("/stream").send().await.text(), "1,2,3,");
}

#[tokio::test]
async fn middleware_and_body_limits_apply() {
    let app = App::new()
        .max_body_size(Some(8))
        .middleware(|req: Request, next: Next| async move {
            Ok(next.run(req).await?.with_header("x-mw", "1"))
        })
        .post("/echo", |req: Request| async move {
            Ok(Response::text(
                String::from_utf8_lossy(&req.body).into_owned(),
            ))
        });
    let c = TestClient::new(app);
    let ok = c.post("/echo").body("tiny").send().await;
    assert_eq!((ok.status(), ok.header("x-mw")), (200, "1"));
    let big = c.post("/echo").body("far too large").send().await;
    assert_eq!(big.status(), 413);
}

#[tokio::test]
async fn openapi_document_is_published() {
    use ullun::openapi::OpenApiInfo;
    let c = TestClient::new(app().openapi(OpenApiInfo::new("T", "1")));
    let spec = c
        .get("/openapi.json")
        .send()
        .await
        .json::<serde_json::Value>();
    assert!(spec["paths"]["/hello/{name}"].is_object());
}

#[tokio::test]
async fn clients_are_cheap_to_clone_and_share_the_app() {
    let c = TestClient::new(app());
    let d = c.clone();
    let (a, b) = tokio::join!(c.get("/state").send(), d.get("/state").send());
    assert_eq!((a.text(), b.text()), ("42".into(), "42".into()));
}
