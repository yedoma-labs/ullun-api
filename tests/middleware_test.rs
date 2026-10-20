//! Middleware behaviour over real HTTP on ephemeral ports

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use ullun::prelude::*;

async fn spawn(app: App) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

async fn send(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
) -> (u16, hyper::HeaderMap, String) {
    send_with(addr, method, path, &[]).await
}

async fn send_with(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, hyper::HeaderMap, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let req = builder.body(Full::<Bytes>::default()).unwrap();
    let res = sender.send_request(req).await.unwrap();
    let (status, headers) = (res.status().as_u16(), res.headers().clone());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

async fn tag(req: Request, next: Next) -> Result<Response> {
    let res = match next.run(req).await {
        Ok(res) => res,
        Err(err) => Response::from_error(err),
    };
    Ok(res.with_header("x-tagged", "yes"))
}

#[tokio::test]
async fn cors_preflight_is_answered_without_a_matching_route() {
    let addr = spawn(
        App::new()
            .middleware(ullun::middleware::cors(vec![
                "https://app.example.com".into()
            ]))
            .get("/data", |_req: Request| async { Ok(Response::text("x")) }),
    )
    .await;

    let (status, headers, _) = send(addr, "OPTIONS", "/data").await;
    assert_eq!(status, 204);
    assert_eq!(
        headers["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert!(headers.contains_key("access-control-allow-methods"));
}

#[tokio::test]
async fn middleware_wraps_not_found_responses() {
    let addr = spawn(App::new().middleware(tag)).await;

    let (status, headers, body) = send(addr, "GET", "/missing").await;
    assert_eq!(status, 404);
    assert_eq!(headers["x-tagged"], "yes");
    assert!(body.contains("Route not found"));

    // unsupported methods end in the same 404
    let (status, headers, _) = send(addr, "TRACE", "/missing").await;
    assert_eq!(status, 404);
    assert_eq!(headers["x-tagged"], "yes");
}

#[tokio::test]
async fn middleware_runs_in_registration_order() {
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let (a, b) = (order.clone(), order.clone());
    let addr = spawn(
        App::new()
            .middleware(move |req: Request, next: Next| {
                let a = a.clone();
                async move {
                    a.lock().unwrap().push("a");
                    next.run(req).await
                }
            })
            .middleware(move |req: Request, next: Next| {
                let b = b.clone();
                async move {
                    b.lock().unwrap().push("b");
                    next.run(req).await
                }
            })
            .get("/", |_req: Request| async { Ok(Response::text("ok")) }),
    )
    .await;

    assert_eq!(send(addr, "GET", "/").await.0, 200);
    assert_eq!(*order.lock().unwrap(), ["a", "b"]);
}

#[tokio::test]
async fn group_middleware_only_wraps_group_routes() {
    async fn admin_only(req: Request, next: Next) -> Result<Response> {
        if req.headers.get("x-admin").is_none() {
            return Err(Error::forbidden("admin only"));
        }
        next.run(req).await
    }

    let addr = spawn(
        App::new()
            .middleware(tag)
            .get("/public", |_req: Request| async {
                Ok(Response::text("public"))
            })
            .group("/admin", |g| {
                // registered before the routes: still applies to all of them
                g.middleware(admin_only)
                    .get("/panel", |_req: Request| async {
                        Ok(Response::text("panel"))
                    })
            }),
    )
    .await;

    assert_eq!(send(addr, "GET", "/public").await.0, 200);
    let (status, headers, _) = send(addr, "GET", "/admin/panel").await;
    assert_eq!(status, 403);
    // global middleware still wraps the group's rejection
    assert_eq!(headers["x-tagged"], "yes");
    let (status, _, body) = send_with(addr, "GET", "/admin/panel", &[("x-admin", "1")]).await;
    assert_eq!((status, body.as_str()), (200, "panel"));
}

#[derive(Clone, Debug, PartialEq)]
struct CurrentUser(String);

#[tokio::test]
async fn middleware_passes_data_via_extensions_and_peer_addr_is_set() {
    let addr = spawn(
        App::new()
            .middleware(|mut req: Request, next: Next| async move {
                req.extensions.insert(CurrentUser("ann".into()));
                next.run(req).await
            })
            .get("/me", |req: Request| async move {
                let user = req.extensions.get::<CurrentUser>().cloned();
                let peer = req.remote_addr.map(|a| a.ip().to_string());
                Ok(Response::json(serde_json::json!({
                    "user": user.map(|u| u.0),
                    "peer": peer,
                })))
            }),
    )
    .await;

    let (status, _, body) = send(addr, "GET", "/me").await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"peer":"127.0.0.1","user":"ann"}"#);
}
