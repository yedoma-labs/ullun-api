//! End-to-end integration tests with real HTTP requests

use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::TcpStream;
use ullun::prelude::*;

#[tokio::test]
async fn test_hello_world() {
    // Spawn server in background
    let server = tokio::spawn(async {
        App::new()
            .get("/", |_req: ullun::request::Request| async {
                Ok(Response::text("Hello, World!"))
            })
            .run("127.0.0.1:3001")
            .await
    });

    // Wait for server to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Make request
    let stream = TcpStream::connect("127.0.0.1:3001").await.unwrap();
    let io = TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            eprintln!("Connection failed: {:?}", err);
        }
    });

    let request = Request::builder()
        .uri("http://127.0.0.1:3001/")
        .body(String::new())
        .unwrap();

    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    let body = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert_eq!(body, "Hello, World!");

    // Cleanup
    server.abort();
}

#[tokio::test]
async fn test_path_params() {
    let server = tokio::spawn(async {
        App::new()
            .get_with_params("/users/{id}", |params| async move {
                let id = params.get("id")?;
                Ok(Response::json(json!({ "user_id": id })))
            })
            .run("127.0.0.1:3002")
            .await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream = TcpStream::connect("127.0.0.1:3002").await.unwrap();
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    
    tokio::task::spawn(async move {
        let _ = conn.await;
    });

    let request = Request::builder()
        .uri("http://127.0.0.1:3002/users/123")
        .body(String::new())
        .unwrap();

    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["user_id"], "123");

    server.abort();
}

#[tokio::test]
async fn test_404_not_found() {
    let server = tokio::spawn(async {
        App::new()
            .get("/exists", |_req: ullun::request::Request| async {
                Ok(Response::text("OK"))
            })
            .run("127.0.0.1:3003")
            .await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream = TcpStream::connect("127.0.0.1:3003").await.unwrap();
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    
    tokio::task::spawn(async move {
        let _ = conn.await;
    });

    let request = Request::builder()
        .uri("http://127.0.0.1:3003/does-not-exist")
        .body(String::new())
        .unwrap();

    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    server.abort();
}

#[tokio::test]
async fn test_json_post() {
    let server = tokio::spawn(async {
        App::new()
            .post("/echo", |req: ullun::request::Request| async move {
                let body: serde_json::Value = req.json()?;
                Ok(Response::json(body))
            })
            .run("127.0.0.1:3004")
            .await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream = TcpStream::connect("127.0.0.1:3004").await.unwrap();
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    
    tokio::task::spawn(async move {
        let _ = conn.await;
    });

    let payload = json!({"name": "Alice", "age": 30});
    let body_str = serde_json::to_string(&payload).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("http://127.0.0.1:3004/echo")
        .header("content-type", "application/json")
        .body(body_str)
        .unwrap();

    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["name"], "Alice");
    assert_eq!(body["age"], 30);

    server.abort();
}

#[tokio::test]
async fn test_middleware_execution() {
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let call_log = Arc::new(Mutex::new(Vec::new()));
    let log_clone = call_log.clone();

    let server = tokio::spawn(async move {
        let log = log_clone;
        
        App::new()
            .middleware(move |req: ullun::request::Request, next| {
                let log = log.clone();
                async move {
                    log.lock().await.push("before");
                    let response = next.run(req).await?;
                    log.lock().await.push("after");
                    Ok(response)
                }
            })
            .get("/", |_req: ullun::request::Request| async {
                Ok(Response::text("OK"))
            })
            .run("127.0.0.1:3005")
            .await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream = TcpStream::connect("127.0.0.1:3005").await.unwrap();
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    
    tokio::task::spawn(async move {
        let _ = conn.await;
    });

    let request = Request::builder()
        .uri("http://127.0.0.1:3005/")
        .body(String::new())
        .unwrap();

    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify middleware was called
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let log = call_log.lock().await;
    assert_eq!(log.as_slice(), &["before", "after"]);

    server.abort();
}
