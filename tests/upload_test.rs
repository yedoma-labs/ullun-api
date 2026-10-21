//! Streaming request bodies

mod common;

use common::spawn;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use ullun::prelude::*;
use ullun::testing::TestClient;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ullun-upload-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn app(dir: PathBuf) -> App {
    let d1 = dir.clone();
    let d2 = dir;
    App::new()
        .max_body_size(Some(1024)) // tiny default, streaming routes ignore it
        .stream_body("/upload", Some(2 * 1024 * 1024))
        .stream_body("/upload/unlimited", None)
        .put("/upload/{name}", move |mut req: Request| {
            let dir = d1.clone();
            async move {
                let name = req.params.get("name")?.to_string();
                let mut body = req.body_stream()?;
                let declared = body.content_length();
                let n = body.save_to(dir.join(&name)).await?;
                Ok(Response::json(
                    serde_json::json!({ "stored": n, "declared": declared }),
                ))
            }
        })
        .put("/upload/unlimited/{name}", move |mut req: Request| {
            let dir = d2.clone();
            async move {
                let name = req.params.get("name")?.to_string();
                let n = req.body_stream()?.save_to(dir.join(name)).await?;
                Ok(Response::text(n.to_string()))
            }
        })
        .post("/buffered", |mut req: Request| async move {
            // works on ordinary routes too: the buffered body arrives as one chunk
            let all = req.body_stream()?.collect().await?;
            Ok(Response::text(all.len().to_string()))
        })
        .post("/upload/json", |req: Request| async move {
            // streaming routes see no buffered body
            Ok(Response::text(req.body.len().to_string()))
        })
        .post("/count", |mut req: Request| async move {
            let mut s = req.body_stream()?;
            let mut chunks = 0;
            while s.chunk().await?.is_some() {
                chunks += 1;
            }
            Ok(Response::text(format!(
                "{} bytes in {} chunks",
                s.bytes_read(),
                chunks
            )))
        })
        .stream_body("/count", None)
}

#[tokio::test]
async fn bodies_larger_than_the_buffer_limit_are_streamed_to_disk() {
    let d = dir("big");
    let s = spawn(app(d.clone())).await;
    let data = vec![b'x'; 1_500_000]; // far above max_body_size, below the stream cap
    let res = s
        .request("PUT", "/upload/big.bin")
        .body(data.clone())
        .send()
        .await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json()["stored"], 1_500_000);
    assert_eq!(res.json()["declared"], 1_500_000);
    assert_eq!(std::fs::read(d.join("big.bin")).unwrap(), data);
}

#[tokio::test]
async fn the_stream_limit_answers_413_and_removes_the_partial_file() {
    let d = dir("limit");
    let s = spawn(app(d.clone())).await;
    let res = s
        .request("PUT", "/upload/toobig.bin")
        .body(vec![b'x'; 3 * 1024 * 1024])
        .send()
        .await;
    assert_eq!(res.status, 413);
    assert!(!d.join("toobig.bin").exists());
}

#[tokio::test]
async fn a_longer_prefix_can_lift_the_limit() {
    let d = dir("unlimited");
    let s = spawn(app(d.clone())).await;
    let res = s
        .request("PUT", "/upload/unlimited/huge.bin")
        .body(vec![b'y'; 3 * 1024 * 1024])
        .send()
        .await;
    assert_eq!((res.status, res.text().as_str()), (200, "3145728"));
}

#[tokio::test]
async fn ordinary_routes_keep_their_buffer_limit_and_can_still_use_the_stream_api() {
    let s = spawn(app(dir("plain"))).await;
    let ok = s.post("/buffered").body(vec![b'z'; 1000]).send().await;
    assert_eq!(ok.text(), "1000");
    let too_big = s.post("/buffered").body(vec![b'z'; 2000]).send().await;
    assert_eq!(too_big.status, 413);
    let json = s.post("/upload/json").body(b"hello".to_vec()).send().await;
    assert_eq!(json.text(), "0");
}

#[tokio::test]
async fn chunks_arrive_as_the_client_sends_them() {
    let s = spawn(app(dir("chunks"))).await;
    let mut tcp = TcpStream::connect(s.addr).await.unwrap();
    tcp.write_all(
        b"POST /count HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    for part in ["hello", "streaming", "world"] {
        tcp.write_all(format!("{:x}\r\n{part}\r\n", part.len()).as_bytes())
            .await
            .unwrap();
        tcp.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    tcp.write_all(b"0\r\n\r\n").await.unwrap();
    let mut out = String::new();
    tcp.read_to_string(&mut out).await.unwrap();
    assert!(out.contains("200 OK"), "{out}");
    assert!(out.ends_with("19 bytes in 3 chunks"), "{out}");
}

#[tokio::test]
async fn an_interrupted_upload_leaves_no_file_behind() {
    let d = dir("cut");
    let s = spawn(app(d.clone())).await;
    let mut tcp = TcpStream::connect(s.addr).await.unwrap();
    tcp.write_all(
        b"PUT /upload/cut.bin HTTP/1.1\r\nHost: x\r\nContent-Length: 100000\r\n\r\npartial data",
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(tcp); // client vanishes mid-body
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!d.join("cut.bin").exists());
}

#[tokio::test]
async fn a_declared_length_over_the_limit_is_refused_before_reading() {
    let d = dir("declared");
    let s = spawn(app(d.clone())).await;
    let mut tcp = TcpStream::connect(s.addr).await.unwrap();
    tcp.write_all(
        b"PUT /upload/x.bin HTTP/1.1\r\nHost: x\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut out = String::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tcp.read_to_string(&mut out),
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    assert!(!d.join("x.bin").exists());
}

#[tokio::test]
async fn the_test_client_streams_too() {
    let d = dir("testclient");
    let c = TestClient::new(app(d.clone()));
    let res = c.put("/upload/t.bin").body(vec![b'q'; 5000]).send().await;
    assert_eq!(res.status(), 200);
    assert_eq!(std::fs::metadata(d.join("t.bin")).unwrap().len(), 5000);
    let big = c
        .put("/upload/t2.bin")
        .body(vec![b'q'; 3 * 1024 * 1024])
        .send()
        .await;
    assert_eq!(big.status(), 413);
}
