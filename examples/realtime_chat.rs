//! Scenario: real-time chat over WebSockets (feature `websocket`)
//!
//! - HTML page at `/`, WebSocket endpoint at `/ws?name=...`
//! - every message is broadcast to all connected clients
//! - the page renders messages with `textContent` (no HTML injection)
//! - cross-site origins are rejected by default
//!
//! Port: set PORT (default 3000)
//! Run:  cargo run --example realtime_chat --features websocket   then open two browser tabs

use tokio::sync::broadcast;
use ullun::prelude::*;
use ullun::websocket::{Message, WebSocket};

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>ullun chat</title>
<body style="font-family:system-ui;max-width:40rem;margin:2rem auto">
<h1>ullun chat</h1>
<ul id="log" style="list-style:none;padding:0"></ul>
<form id="f"><input id="m" autocomplete="off" autofocus> <button>Send</button></form>
<script>
const name = prompt("Your name?") || "anon";
const ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws?name=${encodeURIComponent(name)}`);
const log = document.getElementById("log");
ws.onmessage = e => { const li = document.createElement("li"); li.textContent = e.data; log.append(li); };
ws.onclose = () => { const li = document.createElement("li"); li.textContent = "(disconnected)"; log.append(li); };
document.getElementById("f").onsubmit = e => { e.preventDefault(); const m = document.getElementById("m"); if (m.value) ws.send(m.value); m.value = ""; };
</script>"#;

fn clean_name(raw: Option<&str>) -> String {
    let name: String = raw
        .unwrap_or("anon")
        .chars()
        .filter(|c| !c.is_control())
        .take(20)
        .collect();
    if name.trim().is_empty() {
        "anon".to_string()
    } else {
        name
    }
}

#[tokio::main]
async fn main() {
    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );
    let (tx, _) = broadcast::channel::<String>(256);

    App::new()
        .get("/", |_req: Request| async { Ok(Response::html(PAGE)) })
        .ws("/ws", move |req: Request, mut socket: WebSocket| {
            let tx = tx.clone();
            async move {
                let name = clean_name(req.query.get("name"));
                let mut rx = tx.subscribe();
                let _ = tx.send(format!("* {name} joined"));

                loop {
                    tokio::select! {
                        incoming = socket.recv() => match incoming {
                            Some(Ok(Message::Text(text))) => {
                                let text: String = text.chars().take(500).collect();
                                let _ = tx.send(format!("{name}: {text}"));
                            }
                            Some(Ok(Message::Close)) | None | Some(Err(_)) => break,
                            Some(Ok(_)) => {}
                        },
                        outgoing = rx.recv() => match outgoing {
                            Ok(line) => {
                                if socket.send_text(line).await.is_err() {
                                    break;
                                }
                            }
                            // slow client missed messages: skip, keep going
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => break,
                        },
                    }
                }
                let _ = tx.send(format!("* {name} left"));
            }
        })
        .run(&addr)
        .await
        .unwrap();
}
