//! RFC 9457 problem details and content negotiation

use ullun::prelude::*;
use ullun::problem::ProblemDetails;
use ullun::testing::TestClient;
use ullun::validate::Validator;

fn app(problem: ProblemDetails) -> App {
    App::new()
        .middleware(problem)
        .get("/missing", |_req: Request| async {
            Err::<Response, _>(Error::not_found("Order 17 does not exist"))
        })
        .post("/orders", |_req: Request| async {
            let mut v = Validator::new();
            v.error("quantity", "must be 1-50");
            v.error("quantity", "must be a number");
            v.error("sku", "unknown");
            v.finish()?;
            Ok(Response::text("ok"))
        })
        .get("/boom", |_req: Request| async {
            Err::<Response, _>(Error::internal("database unavailable"))
        })
        .get("/custom", |_req: Request| async {
            Ok(
                Response::json(serde_json::json!({ "error": "mine", "code": 7 }))
                    .with_status(http::StatusCode::BAD_REQUEST),
            )
        })
        .get("/fine", |_req: Request| async {
            Ok(Response::text("fine"))
        })
        .get("/page", |req: Request| async move {
            Ok(
                match req.accepts(&["application/json", "text/html", "text/csv"]) {
                    Some("text/html") => Response::html("<p>hi</p>"),
                    Some("text/csv") => {
                        Response::text("a,b").with_header("content-type", "text/csv")
                    }
                    Some(_) => Response::json(serde_json::json!({ "hi": true })),
                    None => Response::from_error(Error::custom(406, "Cannot produce that format")),
                },
            )
        })
        .get("/lang", |req: Request| async move {
            Ok(Response::text(
                req.accepts_language(&["en", "de"]).unwrap_or("none"),
            ))
        })
}

#[tokio::test]
async fn not_found_becomes_problem_json() {
    let c = TestClient::new(app(ProblemDetails::new()));
    let res = c.get("/missing").send().await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.header("content-type"), "application/problem+json");
    let p: serde_json::Value = res.json();
    assert_eq!(p["type"], "about:blank");
    assert_eq!(p["title"], "Not Found");
    assert_eq!(p["status"], 404);
    assert_eq!(p["detail"], "Order 17 does not exist");
    assert_eq!(p["instance"], "/missing");
    assert!(p.get("errors").is_none());
}

#[tokio::test]
async fn type_base_and_validation_errors() {
    let c = TestClient::new(app(
        ProblemDetails::new().type_base("https://api.example.com/problems/")
    ));
    let nf = c.get("/nope").send().await.json::<serde_json::Value>();
    assert_eq!(nf["type"], "https://api.example.com/problems/not-found");

    let res = c.post("/orders").send().await;
    assert_eq!(res.status(), 422);
    let p: serde_json::Value = res.json();
    assert_eq!(
        p["type"],
        "https://api.example.com/problems/validation-failed"
    );
    assert_eq!(p["title"], "Unprocessable Entity");
    assert_eq!(p["errors"]["quantity"][0], "must be 1-50");
    assert_eq!(p["errors"]["quantity"][1], "must be a number");
    assert_eq!(p["errors"]["sku"][0], "unknown");
}

#[tokio::test]
async fn router_errors_and_server_errors_are_converted_too() {
    let c = TestClient::new(app(ProblemDetails::new().without_instance()));
    let wrong = c.delete("/fine").send().await;
    assert_eq!(wrong.status(), 405);
    assert_eq!(wrong.header("content-type"), "application/problem+json");
    assert_eq!(wrong.header("allow"), "GET, HEAD, OPTIONS");
    let p: serde_json::Value = c.get("/boom").send().await.json();
    assert_eq!(
        (p["status"].as_u64(), p["title"].as_str()),
        (Some(500), Some("Internal Server Error"))
    );
    assert!(p.get("instance").is_none());
}

#[tokio::test]
async fn other_responses_are_left_alone() {
    let c = TestClient::new(app(ProblemDetails::new()));
    let ok = c.get("/fine").send().await;
    assert_eq!((ok.status(), ok.text().as_str()), (200, "fine"));

    let custom = c.get("/custom").send().await;
    assert_eq!(custom.status(), 400);
    assert_eq!(custom.header("content-type"), "application/json");
    assert_eq!(custom.json::<serde_json::Value>()["code"], 7);
}

#[tokio::test]
async fn content_negotiation_in_handlers() {
    let c = TestClient::new(app(ProblemDetails::new()));
    let html = c
        .get("/page")
        .header("accept", "text/html,application/json;q=0.5")
        .send()
        .await;
    assert!(html.header("content-type").starts_with("text/html"));
    let json = c.get("/page").send().await;
    assert_eq!(json.json::<serde_json::Value>()["hi"], true);
    let csv = c.get("/page").header("accept", "text/csv").send().await;
    assert_eq!(csv.text(), "a,b");
    let none = c.get("/page").header("accept", "image/png").send().await;
    assert_eq!(none.status(), 406);

    assert_eq!(
        c.get("/lang")
            .header("accept-language", "de-CH,de;q=0.9")
            .send()
            .await
            .text(),
        "de"
    );
    assert_eq!(c.get("/lang").send().await.text(), "en");
    assert_eq!(
        c.get("/lang")
            .header("accept-language", "ja")
            .send()
            .await
            .text(),
        "none"
    );
}
