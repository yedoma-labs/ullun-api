#![cfg(feature = "websocket")]
//! WebSocket endpoints against a real client (tokio-tungstenite)

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as Wire};
use ullun::prelude::*;
use ullun::websocket::{Message, WebSocket, WsOptions};

async fn spawn(app: App) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

fn echo_app() -> App {
    App::new()
        .middleware(|mut req: Request, next: Next| async move {
            req.extensions.insert(7u32);
            next.run(req).await
        })
        .ws(
            "/echo/{room}",
            |req: Request, mut socket: WebSocket| async move {
                let room = req.params.get_opt("room").unwrap_or("?").to_string();
                let who = req.query.get("as").unwrap_or("anon").to_string();
                let ext = req.extensions.get::<u32>().copied().unwrap_or(0);
                let _ = socket
                    .send_text(format!("hello {who} in {room} ({ext})"))
                    .await;
                while let Some(Ok(msg)) = socket.recv().await {
                    match msg {
                        Message::Text(t) => {
                            let _ = socket.send_text(t.to_uppercase()).await;
                        }
                        Message::Binary(b) => {
                            let _ = socket.send_binary(b).await;
                        }
                        Message::Close => break,
                        _ => {}
                    }
                }
            },
        )
}

#[tokio::test]
async fn echoes_text_and_binary_with_request_context() {
    let addr = spawn(echo_app()).await;
    let (mut ws, response) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/echo/lobby?as=ann"))
            .await
            .unwrap();
    assert_eq!(response.status(), 101);

    assert_eq!(
        ws.next().await.unwrap().unwrap(),
        Wire::text("hello ann in lobby (7)")
    );
    ws.send(Wire::text("ping me")).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Wire::text("PING ME"));
    ws.send(Wire::binary(vec![1, 2, 3])).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap(),
        Wire::binary(vec![1, 2, 3])
    );

    // protocol-level ping is answered by the library
    ws.send(Wire::Ping(vec![9].into())).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap(),
        Wire::Pong(vec![9].into())
    );

    ws.close(None).await.unwrap();
    // the server finishes the closing handshake
    while let Some(msg) = ws.next().await {
        if matches!(msg, Ok(Wire::Close(_)) | Err(_)) {
            break;
        }
    }
}

#[tokio::test]
async fn plain_http_requests_and_bad_handshakes_are_rejected() {
    let addr = spawn(echo_app()).await;

    // plain GET without upgrade headers
    let plain = raw_get(addr, "/echo/x", &[]).await;
    assert!(plain.starts_with("HTTP/1.1 400"), "{plain}");

    // wrong version
    let old = raw_get(
        addr,
        "/echo/x",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "8"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
    )
    .await;
    assert!(old.starts_with("HTTP/1.1 426"), "{old}");
    assert!(old.to_lowercase().contains("sec-websocket-version: 13"));
}

async fn raw_get(addr: std::net::SocketAddr, path: &str, headers: &[(&str, &str)]) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn cross_site_origin_is_rejected_unless_allowed() {
    let app = App::new()
        .ws("/strict", |_req: Request, mut s: WebSocket| async move {
            let _ = s.send_text("ok").await;
        })
        .ws_with(
            "/open",
            WsOptions {
                allowed_origins: vec!["https://app.example.com".into()],
                ..Default::default()
            },
            |_req: Request, mut s: WebSocket| async move {
                let _ = s.send_text("ok").await;
            },
        );
    let addr = spawn(app).await;

    let connect = |path: &str, origin: Option<String>| {
        let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
        if let Some(o) = origin {
            req.headers_mut().insert("origin", o.parse().unwrap());
        }
        tokio_tungstenite::connect_async(req)
    };

    // no Origin (native client) and same-origin pass
    assert!(connect("/strict", None).await.is_ok());
    assert!(connect("/strict", Some(format!("http://{addr}")))
        .await
        .is_ok());

    // cross-site is a 403 on the strict endpoint
    match connect("/strict", Some("https://evil.example".into())).await {
        Err(WsError::Http(res)) => assert_eq!(res.status(), 403),
        other => panic!("expected 403, got {:?}", other.map(|(_, r)| r.status())),
    }
    // allow-listed origin passes on the other
    assert!(connect("/open", Some("https://app.example.com".into()))
        .await
        .is_ok());
    assert!(connect("/open", Some("https://evil.example".into()))
        .await
        .is_err());
}

#[tokio::test]
async fn oversized_messages_close_the_connection() {
    let app = App::new().ws_with(
        "/small",
        WsOptions {
            max_message_size: 64,
            ..Default::default()
        },
        |_req: Request, mut s: WebSocket| async move {
            while let Some(Ok(msg)) = s.recv().await {
                if let Message::Text(t) = msg {
                    let _ = s.send_text(format!("got {}", t.len())).await;
                }
            }
        },
    );
    let addr = spawn(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/small"))
        .await
        .unwrap();

    ws.send(Wire::text("x".repeat(10))).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Wire::text("got 10"));

    ws.send(Wire::text("x".repeat(1000))).await.unwrap();
    // no echo: the server drops the connection (close frame or reset)
    loop {
        match ws.next().await {
            Some(Ok(Wire::Text(t))) => panic!("oversized message was processed: {t}"),
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => break,
        }
    }
}

fn quick_heartbeat() -> WsOptions {
    WsOptions {
        ping_interval: Some(std::time::Duration::from_millis(30)),
        idle_timeout: Some(std::time::Duration::from_millis(150)),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_server_pings_and_keeps_responsive_clients() {
    let app = App::new().ws_with(
        "/hb",
        quick_heartbeat(),
        |_req: Request, mut s: WebSocket| async move {
            while let Some(Ok(msg)) = s.recv().await {
                if let Message::Text(t) = msg {
                    let _ = s.send_text(format!("echo {t}")).await;
                }
            }
        },
    );
    let addr = spawn(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/hb"))
        .await
        .unwrap();

    // reading answers pings automatically; keep reading well past the idle timeout
    let mut pings = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, ws.next()).await {
        if matches!(msg, Ok(Wire::Ping(_))) {
            pings += 1;
        }
    }
    assert!(pings >= 3, "expected regular pings, saw {pings}");
    // still connected after more than three idle timeouts
    ws.send(Wire::text("still here")).await.unwrap();
    loop {
        match ws.next().await.unwrap().unwrap() {
            Wire::Text(t) => {
                assert_eq!(t.as_str(), "echo still here");
                break;
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn silent_peers_are_disconnected_after_the_idle_timeout() {
    let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
    let ended = std::sync::Arc::new(tokio::sync::Mutex::new(Some(ended_tx)));
    let app = App::new().ws_with(
        "/hb",
        quick_heartbeat(),
        move |_req: Request, mut s: WebSocket| {
            let ended = ended.clone();
            async move {
                while let Some(Ok(_)) = s.recv().await {}
                if let Some(tx) = ended.lock().await.take() {
                    let _ = tx.send(());
                }
            }
        },
    );
    let addr = spawn(app).await;
    // connected, but never polled again: no pongs, no traffic
    let (_ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/hb"))
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), ended_rx)
        .await
        .expect("the silent connection was never closed")
        .unwrap();
}

#[tokio::test]
async fn json_helper_and_close_codes() {
    let app = App::new().ws("/j", |_req: Request, mut s: WebSocket| async move {
        let _ = s
            .send_json(&serde_json::json!({ "type": "hello", "n": 1 }))
            .await;
        let _ = s.close_with(1008, "policy violation").await;
    });
    let addr = spawn(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/j"))
        .await
        .unwrap();

    let first = ws.next().await.unwrap().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(parsed, serde_json::json!({ "type": "hello", "n": 1 }));
    match ws.next().await {
        Some(Ok(Wire::Close(Some(frame)))) => {
            assert_eq!(u16::from(frame.code), 1008);
            assert_eq!(frame.reason.as_str(), "policy violation");
        }
        other => panic!("expected a close frame, got {other:?}"),
    }
}
