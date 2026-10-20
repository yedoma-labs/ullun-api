//! Static file serving against a real server on an ephemeral port

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use std::path::PathBuf;
use tokio::net::{TcpListener, TcpStream};
use ullun::prelude::*;

fn fixture_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ullun-static-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("index.html"), "<h1>home</h1>").unwrap();
    std::fs::write(dir.join("app.css"), "body{}").unwrap();
    std::fs::write(dir.join("sub/index.html"), "<h1>sub</h1>").unwrap();
    std::fs::create_dir_all(dir.join("empty")).unwrap();
    dir
}

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
        .body(Empty::<Bytes>::new())
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    let status = res.status().as_u16();
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, content_type, body)
}

#[tokio::test]
async fn serves_files_under_prefix() {
    let dir = fixture_dir("prefix");
    let addr = spawn(App::new().serve_static("/assets/", dir.to_str().unwrap())).await;

    let (status, ct, body) = get(addr, "/assets/app.css").await;
    assert_eq!(status, 200);
    assert_eq!(ct, "text/css; charset=utf-8");
    assert_eq!(body, "body{}");

    // prefix root and nested directory resolve to index.html
    for path in ["/assets", "/assets/"] {
        let (status, _, body) = get(addr, path).await;
        assert_eq!((status, body.as_str()), (200, "<h1>home</h1>"), "{path}");
    }
    let (status, _, body) = get(addr, "/assets/sub").await;
    assert_eq!((status, body.as_str()), (200, "<h1>sub</h1>"));

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn serves_from_root_next_to_api_routes() {
    let dir = fixture_dir("root");
    let app = App::new()
        .get("/api/ping", |_req: Request| async {
            Ok(Response::text("pong"))
        })
        .serve_static("/", dir.to_str().unwrap());
    let addr = spawn(app).await;

    assert_eq!(get(addr, "/api/ping").await.2, "pong");
    assert_eq!(get(addr, "/app.css").await.0, 200);
    assert_eq!(get(addr, "/").await.2, "<h1>home</h1>");

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn missing_files_and_directories() {
    let dir = fixture_dir("missing");
    let addr = spawn(App::new().serve_static("/s", dir.to_str().unwrap())).await;

    assert_eq!(get(addr, "/s/nope.txt").await.0, 404);
    // directory without index.html: no listing
    assert_eq!(get(addr, "/s/empty").await.0, 403);

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn traversal_never_leaves_root() {
    let dir = fixture_dir("traversal");
    let secret = dir
        .parent()
        .unwrap()
        .join(format!("ullun-secret-{}", std::process::id()));
    std::fs::write(&secret, "top secret").unwrap();
    let addr = spawn(App::new().serve_static("/s", dir.to_str().unwrap())).await;

    let name = secret.file_name().unwrap().to_str().unwrap();
    for path in [
        format!("/s/../{name}"),
        format!("/s/..%2F{name}"),
        format!("/s//{}", secret.display()),
    ] {
        let (status, _, body) = get(addr, &path).await;
        assert_ne!(status, 200, "{path}");
        assert!(!body.contains("top secret"), "{path}");
    }

    std::fs::remove_file(secret).ok();
    std::fs::remove_dir_all(dir).ok();
}
