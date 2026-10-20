//! Streaming bodies and Server-Sent Events over real HTTP

mod common;

use common::spawn;
use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use ullun::prelude::*;
use ullun::sse::{sse, Event};

/// Send a GET and return the response head with its body still unread
async fn open(addr: std::net::SocketAddr, path: &str) -> hyper::Response<Incoming> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    sender
        .send_request(
            hyper::Request::builder()
                .uri(path)
                .header("host", "localhost")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn next_chunk(body: &mut Incoming) -> Option<Bytes> {
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("timed out waiting for a chunk")?
        .unwrap();
    frame.into_data().ok()
}

#[tokio::test]
async fn chunks_reach_the_client_while_the_producer_is_still_running() {
    let (gate_tx, gate_rx) = oneshot::channel::<()>();
    let gate = std::sync::Arc::new(Mutex::new(Some(gate_rx)));
    let server = spawn(App::new().get("/slow", move |_req: Request| {
        let gate = gate.clone();
        async move {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                let _ = tx.send("first\n").await;
                // the second chunk is only sent after the client confirmed the first
                if let Some(rx) = gate.lock().await.take() {
                    let _ = rx.await;
                }
                let _ = tx.send("second\n").await;
            });
            Ok(res.with_header("content-type", "text/plain"))
        }
    }))
    .await;

    let res = open(server.addr, "/slow").await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/plain");
    assert!(!res.headers().contains_key("content-length"));
    let mut body = res.into_body();

    assert_eq!(next_chunk(&mut body).await.unwrap(), "first\n");
    gate_tx.send(()).unwrap();
    assert_eq!(next_chunk(&mut body).await.unwrap(), "second\n");
    // producer dropped its sender: the body ends
    assert!(next_chunk(&mut body).await.is_none());
}

#[tokio::test]
async fn producer_learns_when_the_client_disconnects() {
    let (seen_tx, seen_rx) = oneshot::channel::<&'static str>();
    let seen = std::sync::Arc::new(Mutex::new(Some(seen_tx)));
    let server = spawn(App::new().get("/endless", move |_req: Request| {
        let seen = seen.clone();
        async move {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                let outcome = tokio::select! {
                    _ = tx.closed() => "closed",
                    _ = async {
                        loop {
                            if tx.send("tick\n").await.is_err() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    } => "send failed",
                };
                if let Some(s) = seen.lock().await.take() {
                    let _ = s.send(outcome);
                }
            });
            Ok(res)
        }
    }))
    .await;

    let res = open(server.addr, "/endless").await;
    let mut body = res.into_body();
    assert_eq!(next_chunk(&mut body).await.unwrap(), "tick\n");
    drop(body); // closes the connection

    let outcome = tokio::time::timeout(Duration::from_secs(5), seen_rx)
        .await
        .expect("producer was never told about the disconnect")
        .unwrap();
    assert!(outcome == "closed" || outcome == "send failed");
}

#[tokio::test]
async fn sse_sets_headers_and_encodes_events() {
    let server = spawn(App::new().get("/events", |_req: Request| async {
        let (tx, res) = sse();
        tokio::spawn(async move {
            let _ = tx.send(Event::comment("hello")).await;
            let _ = tx
                .send(Event::data("one\ntwo").event("update").id("1"))
                .await;
            let _ = tx
                .send(Event::json(&serde_json::json!({ "n": 2 })).unwrap())
                .await;
        });
        Ok(res)
    }))
    .await;

    let res = open(server.addr, "/events").await;
    assert_eq!(
        res.headers()["content-type"],
        "text/event-stream; charset=utf-8"
    );
    assert_eq!(res.headers()["cache-control"], "no-cache");
    assert_eq!(res.headers()["x-accel-buffering"], "no");

    let mut text = String::new();
    let mut body = res.into_body();
    while let Some(chunk) = next_chunk(&mut body).await {
        text.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert_eq!(
        text,
        ": hello\n\nevent: update\nid: 1\ndata: one\ndata: two\n\ndata: {\"n\":2}\n\n"
    );
}

#[tokio::test]
async fn middleware_leaves_streams_alone() {
    let server = spawn(App::new().middleware(ullun::conditional::ETags).get(
        "/s",
        |_req: Request| async {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                let _ = tx.send("streamed").await;
            });
            Ok(res.with_header("content-type", "text/plain"))
        },
    ))
    .await;

    // an empty buffered body would have been tagged and answered with 304 for "*"
    let res = server.get("/s").header("if-none-match", "*").send().await;
    assert_eq!(res.status, 200);
    assert_eq!(res.text(), "streamed");
    assert_eq!(res.header("etag"), "");
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn compression_skips_streams() {
    let server = spawn(
        App::new()
            .middleware(ullun::compression::Compression::new().min_size(0))
            .get("/s", |_req: Request| async {
                let (tx, res) = Response::stream();
                tokio::spawn(async move {
                    let _ = tx.send("x".repeat(5000)).await;
                });
                Ok(res.with_header("content-type", "text/plain"))
            }),
    )
    .await;
    let res = server
        .get("/s")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(res.header("content-encoding"), "");
    assert_eq!(res.body.len(), 5000);
}
