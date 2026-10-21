//! Cleartext HTTP/2 with prior knowledge (h2c)

mod common;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use ullun::prelude::*;

fn app(h2c: bool) -> App {
    App::new()
        .h2c(h2c)
        .get("/info", |req: Request| async move {
            Ok(Response::json(serde_json::json!({
                "secure": req.is_secure(),
                "path": req.uri.path(),
            })))
        })
        .post("/echo", |req: Request| async move {
            Ok(Response::text(
                String::from_utf8_lossy(&req.body).into_owned(),
            ))
        })
        .get("/slow", |_req: Request| async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(Response::text("slow"))
        })
}

type Sender = hyper::client::conn::http2::SendRequest<Full<Bytes>>;

async fn h2_client(addr: SocketAddr) -> Sender {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    sender
}

fn request(method: &str, path: &str, body: &str) -> hyper::Request<Full<Bytes>> {
    hyper::Request::builder()
        .method(method)
        .uri(path)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

#[tokio::test]
async fn prior_knowledge_http2_is_served_on_a_plain_port() {
    let server = common::spawn(app(true)).await;
    let mut sender = h2_client(server.addr).await;

    let res = sender
        .send_request(request("GET", "http://localhost/info", ""))
        .await
        .unwrap();
    assert_eq!(res.version(), http::Version::HTTP_2);
    assert_eq!(res.status(), 200);
    let body: serde_json::Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["secure"], false);
    assert_eq!(body["path"], "/info");

    let res = sender
        .send_request(request("POST", "http://localhost/echo", "ping"))
        .await
        .unwrap();
    assert_eq!(res.into_body().collect().await.unwrap().to_bytes(), "ping");
}

#[tokio::test]
async fn requests_are_multiplexed_on_one_connection() {
    let server = common::spawn(app(true)).await;
    let sender = h2_client(server.addr).await;
    let started = std::time::Instant::now();
    let calls = (0..5).map(|_| {
        let mut s = sender.clone();
        tokio::spawn(async move {
            s.send_request(request("GET", "http://localhost/slow", ""))
                .await
                .unwrap()
                .status()
        })
    });
    for call in calls.collect::<Vec<_>>() {
        assert_eq!(call.await.unwrap(), 200);
    }
    // five 100 ms requests in parallel, not one after another
    assert!(started.elapsed() < std::time::Duration::from_millis(450));
}

#[tokio::test]
async fn http1_clients_still_work_with_h2c_enabled() {
    let server = common::spawn(app(true)).await;
    let res = server.get("/info").send().await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json()["path"], "/info");

    // a request line that merely starts like the preface is plain HTTP/1.1
    let mut s = TcpStream::connect(server.addr).await.unwrap();
    s.write_all(
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.ends_with("hi"), "{out}");
}

#[tokio::test]
async fn http2_preface_is_refused_unless_enabled() {
    let server = common::spawn(app(false)).await;
    let mut s = TcpStream::connect(server.addr).await.unwrap();
    s.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), s.read_to_end(&mut out)).await;
    // the HTTP/1 parser rejects it (400) or closes: it never speaks HTTP/2
    let text = String::from_utf8_lossy(&out);
    assert!(out.is_empty() || text.starts_with("HTTP/1.1 4"), "{text}");
}

#[tokio::test]
async fn a_slow_preface_is_still_recognised() {
    let server = common::spawn(app(true)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    // preface in two pieces, 50 ms apart
    stream.write_all(b"PRI * HTT").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    stream.write_all(b"P/2.0\r\n\r\nSM\r\n\r\n").await.unwrap();
    // server answers with its SETTINGS frame: type 0x4 at byte 3 of the frame header
    let mut head = [0u8; 9];
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read_exact(&mut head),
    )
    .await
    .expect("server settings frame")
    .unwrap();
    assert_eq!(head[3], 0x4);
}
