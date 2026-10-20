//! Timeouts, load shedding and connection caps over real sockets

mod common;

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use ullun::limits::{ConcurrencyLimit, Timeout};
use ullun::prelude::*;

async fn slow(_req: Request) -> Result<Response> {
    tokio::time::sleep(Duration::from_millis(400)).await;
    Ok(Response::text("slow"))
}

async fn fast(_req: Request) -> Result<Response> {
    Ok(Response::text("fast"))
}

async fn read_all(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn timeout_turns_slow_handlers_into_504() {
    let app = App::new()
        .middleware(Timeout::new(Duration::from_millis(100)))
        .get("/slow", slow)
        .get("/fast", fast);
    let server = common::spawn(app).await;

    let res = server.get("/slow").send().await;
    assert_eq!(res.status, 504);
    assert_eq!(server.get("/fast").send().await.status, 200);
}

#[tokio::test]
async fn concurrency_limit_sheds_with_retry_after() {
    let app = App::new()
        .middleware(ConcurrencyLimit::new(1).retry_after(3))
        .get("/slow", slow)
        .get("/fast", fast);
    let server = common::spawn(app).await;

    let first = tokio::spawn(async move { server.get("/slow").send().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let shed = server.get("/fast").send().await;
    assert_eq!(shed.status, 503);
    assert_eq!(shed.headers.get("retry-after").unwrap(), "3");

    assert_eq!(first.await.unwrap().status, 200);
    // the slot is free again
    assert_eq!(server.get("/fast").send().await.status, 200);
}

#[tokio::test]
async fn concurrency_limit_queue_absorbs_a_short_burst() {
    let app = App::new()
        .middleware(ConcurrencyLimit::new(1).queue_for(Duration::from_secs(3)))
        .get("/slow", slow);
    let server = common::spawn(app).await;

    let calls: Vec<_> = (0..3)
        .map(|_| tokio::spawn(async move { server.get("/slow").send().await.status }))
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap(), 200);
    }
}

#[tokio::test]
async fn queued_request_gives_up_after_its_wait() {
    let app = App::new()
        .middleware(ConcurrencyLimit::new(1).queue_for(Duration::from_millis(50)))
        .get("/slow", slow);
    let server = common::spawn(app).await;

    let first = tokio::spawn(async move { server.get("/slow").send().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.get("/slow").send().await.status, 503);
    assert_eq!(first.await.unwrap().status, 200);
}

#[tokio::test]
async fn header_read_timeout_drops_slowloris_clients() {
    let app = App::new()
        .header_read_timeout(Duration::from_millis(300))
        .get("/fast", fast);
    let server = common::spawn(app).await;

    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /fast HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    // never finishes the header block
    let started = std::time::Instant::now();
    let reply = read_all(&mut stream).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "connection was not cut"
    );
    assert!(!reply.contains("200 OK"));

    // regular clients are unaffected
    assert_eq!(server.get("/fast").send().await.status, 200);
}

#[tokio::test]
async fn connection_cap_refuses_with_503() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = App::new().max_connections(1).get("/fast", fast);
    tokio::spawn(app.serve(listener));

    // occupy the only slot
    let holder = TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut refused = TcpStream::connect(addr).await.unwrap();
    let reply = read_all(&mut refused).await;
    assert!(reply.starts_with("HTTP/1.1 503"), "got {reply:?}");
    assert!(reply.to_lowercase().contains("retry-after: 1"));

    // slot frees up when the holder leaves
    drop(holder);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut ok = TcpStream::connect(addr).await.unwrap();
    ok.write_all(b"GET /fast HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    assert!(read_all(&mut ok).await.contains("200 OK"));
}

#[tokio::test]
async fn per_ip_cap_limits_one_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = App::new().max_connections_per_ip(2).get("/fast", fast);
    tokio::spawn(app.serve(listener));

    let _a = TcpStream::connect(addr).await.unwrap();
    let _b = TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut third = TcpStream::connect(addr).await.unwrap();
    assert!(read_all(&mut third).await.starts_with("HTTP/1.1 503"));
}
