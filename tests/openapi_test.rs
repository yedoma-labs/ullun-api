//! OpenAPI document served by a running app

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;

async fn spawn(app: App) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, String, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .uri(path)
        .header("host", "localhost")
        .body(Full::<Bytes>::default())
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    let status = res.status().as_u16();
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, ct, String::from_utf8(body.to_vec()).unwrap())
}

fn noop() -> impl Fn(Request) -> std::future::Ready<Result<Response>> + Send + Sync + 'static {
    |_req: Request| std::future::ready(Ok(Response::text("ok")))
}

#[tokio::test]
async fn serves_document_with_routes_registered_after_openapi() {
    let app = App::new()
        .get("/health", noop())
        .openapi(OpenApiInfo::new("Demo", "2.0.0").description("A demo"))
        // registered after .openapi(): still part of the document
        .get("/users/{id}", noop())
        .post("/users", noop())
        .doc(
            "post",
            "/users",
            Operation::new("Create user")
                .request_json(schema::object([("name", schema::string())], ["name"]))
                .response(201, "Created"),
        )
        .group("/v1", |g| {
            g.get("/items/{item}", noop()).doc(
                "GET",
                "/items/{item}",
                Operation::new("Get item").path_param("item", schema::integer(), "Item id"),
            )
        });
    let addr = spawn(app).await;

    let (status, ct, body) = get(addr, "/openapi.json").await;
    assert_eq!((status, ct.as_str()), (200, "application/json"));
    let doc: Value = serde_json::from_str(&body).unwrap();

    assert_eq!(doc["info"]["title"], "Demo");
    assert!(doc["paths"]["/health"]["get"].is_object());
    assert_eq!(
        doc["paths"]["/users/{id}"]["get"]["parameters"][0]["name"],
        "id"
    );
    assert_eq!(doc["paths"]["/users"]["post"]["summary"], "Create user");
    assert_eq!(
        doc["paths"]["/users"]["post"]["responses"]["201"]["description"],
        "Created"
    );
    // group docs are keyed by prefix + path
    let item = &doc["paths"]["/v1/items/{item}"]["get"];
    assert_eq!(item["summary"], "Get item");
    assert_eq!(item["parameters"][0]["schema"]["type"], "integer");
    // the spec route itself is not documented
    assert!(doc["paths"].get("/openapi.json").is_none());
    // swagger page is opt-in
    assert_eq!(get(addr, "/docs").await.0, 404);
}

#[tokio::test]
async fn custom_paths_swagger_ui_and_hidden_static_routes() {
    let dir = std::env::temp_dir().join(format!("ullun-openapi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "x").unwrap();

    let app = App::new()
        .serve_static("/assets", dir.to_str().unwrap())
        .get("/ping", noop())
        .openapi(
            OpenApiInfo::new("T", "1")
                .spec_path("/api/spec.json")
                .swagger_ui("/api/docs"),
        );
    let addr = spawn(app).await;

    let (_, _, body) = get(addr, "/api/spec.json").await;
    let doc: Value = serde_json::from_str(&body).unwrap();
    let paths = doc["paths"].as_object().unwrap();
    assert_eq!(paths.keys().collect::<Vec<_>>(), ["/ping"]);

    let (status, ct, page) = get(addr, "/api/docs").await;
    assert_eq!((status, ct.as_str()), (200, "text/html; charset=utf-8"));
    assert!(page.contains("SwaggerUIBundle") && page.contains(r#"url: "/api/spec.json""#));
    assert_eq!(get(addr, "/openapi.json").await.0, 404);

    std::fs::remove_dir_all(dir).ok();
}

#[cfg(feature = "websocket")]
#[tokio::test]
async fn websocket_routes_are_documented_as_upgrades() {
    use ullun::websocket::WebSocket;
    let app = App::new()
        .ws("/live", |_req: Request, _s: WebSocket| async {})
        .openapi(OpenApiInfo::new("T", "1"));
    let addr = spawn(app).await;
    let doc: Value = serde_json::from_str(&get(addr, "/openapi.json").await.2).unwrap();
    assert!(doc["paths"]["/live"]["get"]["responses"]
        .get("101")
        .is_some());
}
