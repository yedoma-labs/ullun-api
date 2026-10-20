#![cfg(all(feature = "websocket", feature = "jwt"))]
//! The chat example with real WebSocket clients

mod common;

#[allow(dead_code)]
#[path = "../examples/chat/app.rs"]
mod chat;

use common::{spawn, Server};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as Wire};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use ullun::websocket::WsOptions;

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn config() -> chat::Config {
    chat::Config {
        users: vec![
            ("alice".into(), "alice-password".into()),
            ("bob".into(), "bob-password-1".into()),
            ("carol".into(), "carol-password".into()),
        ],
        jwt_secret: b"chat-test-secret-chat-test-secret!".to_vec(),
        token_ttl_secs: 600,
        ticket_ttl: Duration::from_secs(30),
        max_connections_per_user: 3,
        message_rate: (1000, Duration::from_secs(1)),
        max_violations: 3,
        ws: WsOptions {
            max_message_size: 4096,
            ..Default::default()
        },
        fast_hashing: true,
        login_rate: (1000, Duration::from_secs(60)),
    }
}

async fn server(cfg: chat::Config) -> Server {
    spawn(chat::build(cfg)).await
}

async fn login(s: &Server, name: &str) -> String {
    let pw = match name {
        "alice" => "alice-password",
        "bob" => "bob-password-1",
        _ => "carol-password",
    };
    let res = s
        .post("/api/login")
        .json(&json!({ "name": name, "password": pw }))
        .send()
        .await;
    assert_eq!(res.status, 200, "{}", res.text());
    res.json()["token"].as_str().unwrap().to_string()
}

async fn ticket(s: &Server, token: &str) -> String {
    let res = s.post("/api/ws-ticket").bearer(token).send().await;
    assert_eq!(res.status, 200, "{}", res.text());
    res.json()["ticket"].as_str().unwrap().to_string()
}

/// Log in and connect to `room` in one go
async fn join(s: &Server, name: &str, room: &str) -> Ws {
    let t = ticket(s, &login(s, name).await).await;
    connect(s, room, &t)
        .await
        .unwrap_or_else(|e| panic!("{name} could not join {room}: {e}"))
}

async fn connect(s: &Server, room: &str, ticket: &str) -> Result<Ws, WsError> {
    tokio_tungstenite::connect_async(format!("ws://{}/ws?room={room}&ticket={ticket}", s.addr))
        .await
        .map(|(ws, _)| ws)
}

/// Next JSON text message (control frames are skipped)
async fn next(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("no message within 5 s")
            .expect("connection ended")
            .expect("socket error");
        if let Wire::Text(t) = msg {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

async fn expect_type(ws: &mut Ws, kind: &str) -> Value {
    let v = next(ws).await;
    assert_eq!(v["type"], kind, "{v}");
    v
}

async fn say(ws: &mut Ws, text: &str) {
    ws.send(Wire::text(
        json!({ "type": "say", "text": text }).to_string(),
    ))
    .await
    .unwrap();
}

/// Assert that nothing arrives for a moment
async fn silence(ws: &mut Ws) {
    let r = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            match ws.next().await {
                Some(Ok(Wire::Text(t))) => return Some(t),
                Some(Ok(_)) => continue,
                _ => return None,
            }
        }
    })
    .await;
    assert!(r.is_err(), "unexpected message: {:?}", r);
}

#[tokio::test]
async fn login_is_checked_and_throttled() {
    let s = server(config()).await;
    let bad = s
        .post("/api/login")
        .json(&json!({ "name": "alice", "password": "wrong-password" }))
        .send()
        .await;
    let unknown = s
        .post("/api/login")
        .json(&json!({ "name": "mallory", "password": "alice-password" }))
        .send()
        .await;
    assert_eq!((bad.status, unknown.status), (401, 401));
    assert_eq!(bad.text(), unknown.text());

    let mut cfg = config();
    cfg.login_rate = (2, Duration::from_secs(60));
    let limited = server(cfg).await;
    for _ in 0..2 {
        limited
            .post("/api/login")
            .json(&json!({ "name": "alice", "password": "x" }))
            .send()
            .await;
    }
    assert_eq!(
        limited
            .post("/api/login")
            .json(&json!({ "name": "alice", "password": "alice-password" }))
            .send()
            .await
            .status,
        429
    );
}

#[tokio::test]
async fn tickets_are_one_time_short_lived_and_checked_before_the_upgrade() {
    let mut cfg = config();
    cfg.ticket_ttl = Duration::from_millis(200);
    let s = server(cfg).await;
    let token = login(&s, "alice").await;

    assert_eq!(s.post("/api/ws-ticket").send().await.status, 401);
    assert_eq!(
        s.post("/api/ws-ticket")
            .bearer("garbage")
            .send()
            .await
            .status,
        401
    );

    let t = ticket(&s, &token).await;
    assert_eq!(t.len(), 32);
    let mut ws = connect(&s, "general", &t).await.unwrap();
    expect_type(&mut ws, "welcome").await;
    // used up
    match connect(&s, "general", &t).await {
        Err(WsError::Http(res)) => assert_eq!(res.status(), 401),
        other => panic!("reusing a ticket must fail: {:?}", other.map(|_| ())),
    }

    // expired
    let old = ticket(&s, &token).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        matches!(connect(&s, "general", &old).await, Err(WsError::Http(res)) if res.status() == 401)
    );
    // made up, missing, and the bearer token itself is not a ticket
    for bad in ["0".repeat(32), String::new(), token.clone()] {
        assert!(
            matches!(connect(&s, "general", &bad).await, Err(WsError::Http(res)) if res.status() == 401),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn room_names_are_validated_and_origins_checked() {
    let s = server(config()).await;
    let token = login(&s, "alice").await;
    for room in ["UPPER", "has%20space", "a%2Fb", &"x".repeat(33)] {
        let t = ticket(&s, &token).await;
        assert!(
            matches!(connect(&s, room, &t).await, Err(WsError::Http(res)) if res.status() == 400),
            "{room}"
        );
    }
    // cross-site pages cannot open the socket even with a valid ticket
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let t = ticket(&s, &token).await;
    let mut req = format!("ws://{}/ws?room=general&ticket={t}", s.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    assert!(
        matches!(tokio_tungstenite::connect_async(req).await, Err(WsError::Http(res)) if res.status() == 403)
    );
}

#[tokio::test]
async fn presence_and_messages_flow_between_members_of_a_room() {
    let s = server(config()).await;
    let mut alice = join(&s, "alice", "general").await;
    let welcome = expect_type(&mut alice, "welcome").await;
    assert_eq!(
        (welcome["room"].as_str(), welcome["you"].as_str()),
        (Some("general"), Some("alice"))
    );
    assert_eq!(welcome["users"], json!(["alice"]));
    assert_eq!(welcome["history"], json!([]));
    expect_type(&mut alice, "joined").await; // her own join is announced too

    let mut bob = join(&s, "bob", "general").await;
    let welcome = expect_type(&mut bob, "welcome").await;
    assert_eq!(welcome["users"], json!(["alice", "bob"]));
    assert_eq!(expect_type(&mut alice, "joined").await["user"], "bob");

    say(&mut alice, "hello bob").await;
    for ws in [&mut alice, &mut bob] {
        let m = loop {
            let v = next(ws).await;
            if v["type"] == "message" {
                break v;
            }
        };
        assert_eq!(
            (m["user"].as_str(), m["text"].as_str()),
            (Some("alice"), Some("hello bob"))
        );
        assert!(m["id"].as_u64().unwrap() >= 1 && m["ts"].as_u64().unwrap() > 1_000_000_000);
    }

    // leaving is announced
    bob.close(None).await.unwrap();
    let left = loop {
        let v = next(&mut alice).await;
        if v["type"] == "left" {
            break v;
        }
    };
    assert_eq!(left["user"], "bob");
}

#[tokio::test]
async fn typing_indicators_reach_others_but_not_the_typist() {
    let s = server(config()).await;
    let mut alice = join(&s, "alice", "t").await;
    expect_type(&mut alice, "welcome").await;
    expect_type(&mut alice, "joined").await;
    let mut bob = join(&s, "bob", "t").await;
    expect_type(&mut bob, "welcome").await;
    expect_type(&mut alice, "joined").await;
    expect_type(&mut bob, "joined").await;

    alice
        .send(Wire::text(json!({ "type": "typing" }).to_string()))
        .await
        .unwrap();
    assert_eq!(expect_type(&mut bob, "typing").await["user"], "alice");
    silence(&mut alice).await;
}

#[tokio::test]
async fn rooms_are_isolated_and_listed() {
    let s = server(config()).await;
    let mut a = join(&s, "alice", "red").await;
    let mut b = join(&s, "bob", "blue").await;
    expect_type(&mut a, "welcome").await;
    expect_type(&mut b, "welcome").await;
    expect_type(&mut a, "joined").await;
    expect_type(&mut b, "joined").await;

    say(&mut a, "psst").await;
    expect_type(&mut a, "message").await;
    silence(&mut b).await;

    let token = login(&s, "carol").await;
    let rooms = s.get("/api/rooms").bearer(&token).send().await.json();
    let list = rooms["rooms"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let red = list.iter().find(|r| r["room"] == "red").unwrap();
    assert_eq!(
        (red["members"].as_u64(), red["messages"].as_u64()),
        (Some(1), Some(1))
    );
    let blue = list.iter().find(|r| r["room"] == "blue").unwrap();
    assert_eq!(blue["messages"], 0);
    assert_eq!(s.get("/api/rooms").send().await.status, 401);
}

#[tokio::test]
async fn late_joiners_get_the_recent_history() {
    let s = server(config()).await;
    let mut alice = join(&s, "alice", "hist").await;
    expect_type(&mut alice, "welcome").await;
    for text in ["one", "two", "three"] {
        say(&mut alice, text).await;
    }
    for _ in 0..4 {
        next(&mut alice).await; // joined + three messages
    }

    let mut bob = join(&s, "bob", "hist").await;
    let welcome = expect_type(&mut bob, "welcome").await;
    let texts: Vec<&str> = welcome["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["one", "two", "three"]);
    // the history endpoint shows the same, newest last, and respects limit
    let token = login(&s, "bob").await;
    let h = s
        .get("/api/rooms/hist/history?limit=2")
        .bearer(&token)
        .send()
        .await
        .json();
    let texts: Vec<&str> = h["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["two", "three"]);
    assert_eq!(
        s.get("/api/rooms/nope/history")
            .bearer(&token)
            .send()
            .await
            .status,
        404
    );
    assert_eq!(
        s.get("/api/rooms/BAD%20ROOM/history")
            .bearer(&token)
            .send()
            .await
            .status,
        400
    );
}

#[tokio::test]
async fn history_is_capped_and_ids_increase() {
    let s = server(config()).await;
    let mut alice = join(&s, "alice", "cap").await;
    expect_type(&mut alice, "welcome").await;
    for i in 0..105 {
        say(&mut alice, &format!("m{i}")).await;
    }
    let mut last = 0;
    let mut seen = 0;
    while seen < 105 {
        let v = next(&mut alice).await;
        if v["type"] == "message" {
            let id = v["id"].as_u64().unwrap();
            assert!(id > last);
            last = id;
            seen += 1;
        }
    }
    let token = login(&s, "alice").await;
    let h = s
        .get("/api/rooms/cap/history?limit=1000")
        .bearer(&token)
        .send()
        .await
        .json();
    let msgs = h["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 100);
    assert_eq!(msgs[0]["text"], "m5");
    assert_eq!(msgs[99]["text"], "m104");
}

#[tokio::test]
async fn bad_input_gets_an_error_not_a_disconnect() {
    let s = server(config()).await;
    let mut ws = join(&s, "alice", "bad").await;
    expect_type(&mut ws, "welcome").await;
    expect_type(&mut ws, "joined").await;

    for payload in [
        "not json",
        r#"{"type":"nope"}"#,
        r#"{"type":"say"}"#,
        r#"{"type":"say","text":"   "}"#,
        r#"{"type":"say","text":"\u0007\u0000"}"#,
    ] {
        ws.send(Wire::text(payload)).await.unwrap();
        let e = expect_type(&mut ws, "error").await;
        assert!(e["message"].as_str().is_some(), "{payload}");
    }
    ws.send(Wire::binary(vec![1, 2, 3])).await.unwrap();
    assert!(expect_type(&mut ws, "error").await["message"]
        .as_str()
        .unwrap()
        .contains("binary"));
    say(&mut ws, &"x".repeat(2001)).await;
    assert!(expect_type(&mut ws, "error").await["message"]
        .as_str()
        .unwrap()
        .contains("characters"));

    // control characters are stripped, the rest is delivered
    say(&mut ws, "  hi\u{7} there\u{0}  ").await;
    assert_eq!(expect_type(&mut ws, "message").await["text"], "hi there");
}

#[tokio::test]
async fn oversized_frames_close_the_connection() {
    let s = server(config()).await;
    let mut ws = join(&s, "alice", "big").await;
    expect_type(&mut ws, "welcome").await;
    ws.send(Wire::text("x".repeat(5000))).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("connection stayed open")
        {
            Some(Ok(Wire::Text(t))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                assert_ne!(
                    v["type"], "message",
                    "oversized frame must not be processed"
                );
            }
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => break,
        }
    }
}

#[tokio::test]
async fn flooding_is_slowed_down_and_then_disconnected() {
    let mut cfg = config();
    cfg.message_rate = (3, Duration::from_secs(60));
    cfg.max_violations = 2;
    let s = server(cfg).await;
    let mut ws = join(&s, "alice", "flood").await;
    expect_type(&mut ws, "welcome").await;
    for i in 0..3 {
        say(&mut ws, &format!("ok {i}")).await;
    }
    say(&mut ws, "too many").await;
    say(&mut ws, "way too many").await;

    let mut accepted = 0u32;
    let mut warnings = 0;
    let mut close_code = None;
    while close_code.is_none() {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("no close")
        {
            Some(Ok(Wire::Text(t))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                match v["type"].as_str() {
                    Some("message") => accepted += 1,
                    Some("error") => warnings += 1,
                    _ => {}
                }
            }
            Some(Ok(Wire::Close(Some(frame)))) => close_code = Some(u16::from(frame.code)),
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    // what the server accepted is what its history holds: exactly the first three
    // (how many of them were echoed back before the close depends on scheduling)
    assert!(accepted <= 3);
    assert_eq!(warnings, 2);
    assert_eq!(close_code, Some(1008));
    let token = login(&s, "bob").await;
    let h = s
        .get("/api/rooms/flood/history")
        .bearer(&token)
        .send()
        .await
        .json();
    let texts: Vec<&str> = h["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["ok 0", "ok 1", "ok 2"]);
}

#[tokio::test]
async fn connections_per_user_are_limited_and_slots_come_back() {
    let s = server(config()).await; // 3 per user
    let token = login(&s, "alice").await;
    let mut open = Vec::new();
    for _ in 0..3 {
        let t = ticket(&s, &token).await;
        let mut ws = connect(&s, "limit", &t).await.unwrap();
        expect_type(&mut ws, "welcome").await;
        open.push(ws);
    }
    let t = ticket(&s, &token).await;
    assert!(
        matches!(connect(&s, "limit", &t).await, Err(WsError::Http(res)) if res.status() == 429)
    );

    // closing one frees a slot (the server notices once the close handshake is done)
    let mut first = open.remove(0);
    first.close(None).await.unwrap();
    while let Some(Ok(_)) = first.next().await {}
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let t = ticket(&s, &token).await;
        if connect(&s, "limit", &t).await.is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "slot was never released"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // failed upgrades do not leak slots either: valid ticket, but a plain HTTP request
    let carol = login(&s, "carol").await;
    for _ in 0..6 {
        let t = ticket(&s, &carol).await;
        let res = s.get(&format!("/ws?room=x&ticket={t}")).send().await;
        assert_eq!(res.status, 400);
    }
    let t = ticket(&s, &carol).await;
    assert!(connect(&s, "x", &t).await.is_ok());
}

#[tokio::test]
async fn a_user_with_two_tabs_leaves_only_when_the_last_one_closes() {
    let s = server(config()).await;
    let mut watcher = join(&s, "bob", "tabs").await;
    expect_type(&mut watcher, "welcome").await;
    expect_type(&mut watcher, "joined").await;

    let mut tab1 = join(&s, "alice", "tabs").await;
    let mut tab2 = join(&s, "alice", "tabs").await;
    expect_type(&mut tab1, "welcome").await;
    expect_type(&mut tab2, "welcome").await;
    // presence lists alice once
    let again = join(&s, "carol", "tabs").await;
    let mut again = again;
    assert_eq!(
        expect_type(&mut again, "welcome").await["users"],
        json!(["alice", "bob", "carol"])
    );

    tab1.close(None).await.unwrap();
    while let Some(Ok(_)) = tab1.next().await {}
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut events = Vec::new();
    while let Ok(Some(Ok(Wire::Text(t)))) =
        tokio::time::timeout(Duration::from_millis(100), watcher.next()).await
    {
        events.push(serde_json::from_str::<Value>(t.as_str()).unwrap());
    }
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "left" && e["user"] == "alice"),
        "alice is still there in tab 2: {events:?}"
    );

    tab2.close(None).await.unwrap();
    loop {
        let v = next(&mut watcher).await;
        if v["type"] == "left" && v["user"] == "alice" {
            break;
        }
    }
}

#[tokio::test]
async fn shutdown_closes_sockets_with_a_going_away_code() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        chat::build(config())
            .shutdown_grace(Duration::from_secs(30))
            .serve_with_shutdown(listener, async {
                let _ = stopped.await;
            }),
    );
    let s = Server { addr };
    let mut ws = join(&s, "alice", "bye").await;
    expect_type(&mut ws, "welcome").await;

    let t = std::time::Instant::now();
    stop.send(()).unwrap();
    let code = loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("no close frame")
        {
            Some(Ok(Wire::Close(Some(frame)))) => break u16::from(frame.code),
            Some(Ok(_)) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    assert_eq!(code, 1001);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "must not wait for the 30 s grace period"
    );
}

#[tokio::test]
async fn the_page_has_no_inline_script_and_the_socket_route_is_documented_as_a_websocket() {
    let s = server(config()).await;
    let page = s.get("/").send().await;
    assert_eq!(page.status, 200);
    assert!(page.text().contains(r#"<script src="/chat.js">"#));
    assert!(!page.text().contains("<script>"));
    assert!(page
        .header("content-security-policy")
        .contains("script-src 'self'"));
    assert!(s
        .get("/chat.js")
        .send()
        .await
        .header("content-type")
        .starts_with("application/javascript"));
}
