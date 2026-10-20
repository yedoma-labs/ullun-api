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
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .body(Full::<Bytes>::default())
        .unwrap();
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
