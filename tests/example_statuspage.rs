//! The status page example, tested over real HTTP against real (local) targets

mod common;

#[allow(dead_code)]
#[path = "../examples/statuspage/app.rs"]
mod statuspage;

use common::{spawn, Server};
use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use ullun::prelude::*;

const KEY: &str = "test-admin-key-0123456789";

fn config() -> statuspage::Config {
    statuspage::Config {
        admin_key: KEY.into(),
        allow_private_targets: true,
        // scheduler effectively off: tests trigger checks explicitly
        tick: Duration::from_secs(3600),
        interval_unit: Duration::from_secs(1),
        failure_threshold: 2,
        check_timeout: Duration::from_millis(500),
    }
}

async fn status_app(config: statuspage::Config) -> Server {
    spawn(statuspage::build(config)).await
}

/// A target whose `/health` can be switched between 200 and 503
async fn flaky_target() -> (Server, Arc<AtomicBool>) {
    let healthy = Arc::new(AtomicBool::new(true));
    let flag = healthy.clone();
    let server = spawn(App::new().get("/health", move |_req: Request| {
        let flag = flag.clone();
        async move {
            if flag.load(Ordering::SeqCst) {
                Ok(Response::text("fine"))
            } else {
                Ok(Response::text("broken").with_status(http::StatusCode::SERVICE_UNAVAILABLE))
            }
        }
    }))
    .await;
    (server, healthy)
}

async fn add(s: &Server, body: Value) -> common::Reply {
    s.post("/api/monitors")
        .header("x-api-key", KEY)
        .json(&body)
        .send()
        .await
}

async fn add_ok(s: &Server, name: &str, target: &str) -> u64 {
    let res = add(s, json!({ "name": name, "target": target })).await;
    assert_eq!(res.status, 201, "{}", res.text());
    res.json()["id"].as_u64().unwrap()
}

async fn check(s: &Server, id: u64) -> common::Reply {
    s.post(&format!("/api/monitors/{id}/check"))
        .header("x-api-key", KEY)
        .send()
        .await
}

async fn status_of(s: &Server, id: u64) -> String {
    let st = s.get("/api/status").send().await.json();
    st["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .map(|m| m["status"].as_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn admin_api_needs_the_key_but_status_is_public() {
    let s = status_app(config()).await;
    for (method, path) in [
        ("GET", "/api/monitors"),
        ("POST", "/api/monitors"),
        ("DELETE", "/api/monitors/1"),
        ("POST", "/api/monitors/1/check"),
        ("GET", "/api/monitors/1/history"),
    ] {
        assert_eq!(
            s.request(method, path).send().await.status,
            401,
            "{method} {path}"
        );
        assert_eq!(
            s.request(method, path)
                .header("x-api-key", "wrong")
                .send()
                .await
                .status,
            401
        );
    }
    for path in ["/", "/api/status", "/healthz", "/app.js", "/openapi.json"] {
        assert_eq!(s.get(path).send().await.status, 200, "{path}");
    }
}

#[tokio::test]
async fn monitor_input_is_validated_all_at_once() {
    let s = status_app(config()).await;

    let res = add(
        &s,
        json!({ "name": "", "target": "ftp://example.com", "interval_secs": 0 }),
    )
    .await;
    assert_eq!(res.status, 422);
    let fields = &res.json()["fields"];
    for f in ["name", "target", "interval_secs"] {
        assert!(fields[f].is_array(), "{f}: {fields}");
    }

    for target in [
        "",
        "example.com",
        "http://",
        "https://example.com",
        "tcp://example.com",
        "javascript:alert(1)",
        "file:///etc/passwd",
    ] {
        let res = add(&s, json!({ "name": "x", "target": target })).await;
        assert_eq!(res.status, 422, "target {target:?}");
        assert!(res.json()["fields"]["target"].is_array());
    }
    let too_slow = add(
        &s,
        json!({ "name": "x", "target": "tcp://127.0.0.1:1", "interval_secs": 3601 }),
    )
    .await;
    assert_eq!(too_slow.status, 422);
    // malformed bodies are a 400, not a crash
    let bad = s
        .post("/api/monitors")
        .header("x-api-key", KEY)
        .text("name=x")
        .send()
        .await;
    assert_eq!(bad.status, 400);
}

#[tokio::test]
async fn private_targets_are_refused_unless_allowed() {
    let strict = status_app(statuspage::Config {
        allow_private_targets: false,
        ..config()
    })
    .await;
    for target in [
        "http://127.0.0.1:8080/",
        "http://localhost/",
        "http://[::1]/",
        "http://10.0.0.5/",
        "http://192.168.1.10/",
        "http://172.16.0.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://0.0.0.0/",
        "http://100.64.0.1/",
        "http://[::ffff:127.0.0.1]/",
        "http://[fd00::1]/",
        "http://[fe80::1]/",
        "tcp://127.0.0.1:5432",
    ] {
        let res = add(&strict, json!({ "name": "ssrf", "target": target })).await;
        assert_eq!(
            res.status,
            422,
            "{target} should be refused: {}",
            res.text()
        );
        let msg = res.json()["fields"]["target"][0]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            msg.contains("not public") || msg.contains("resolution"),
            "{target}: {msg}"
        );
    }
    // public literal addresses are fine (nothing is contacted at creation time)
    assert_eq!(
        add(
            &strict,
            json!({ "name": "ok", "target": "tcp://93.184.216.34:443" })
        )
        .await
        .status,
        201
    );

    let relaxed = status_app(config()).await;
    assert_eq!(
        add(
            &relaxed,
            json!({ "name": "local", "target": "tcp://127.0.0.1:1" })
        )
        .await
        .status,
        201
    );
}

#[tokio::test]
async fn http_monitor_goes_degraded_then_down_then_recovers() {
    let (target, healthy) = flaky_target().await;
    let s = status_app(config()).await;
    let id = add_ok(&s, "API", &format!("http://{}/health", target.addr)).await;
    assert_eq!(status_of(&s, id).await, "unknown");
    assert_eq!(
        s.get("/api/status").send().await.json()["overall"],
        "unknown"
    );

    let up = check(&s, id).await.json();
    assert_eq!(
        (up["ok"].as_bool(), up["detail"].as_str()),
        (Some(true), Some("HTTP 200"))
    );
    assert_eq!(status_of(&s, id).await, "up");
    assert_eq!(
        s.get("/api/status").send().await.json()["overall"],
        "operational"
    );

    healthy.store(false, Ordering::SeqCst);
    let failed = check(&s, id).await.json();
    assert_eq!(
        (failed["ok"].as_bool(), failed["detail"].as_str()),
        (Some(false), Some("HTTP 503"))
    );
    // one failure is only a warning ...
    assert_eq!(status_of(&s, id).await, "degraded");
    assert_eq!(
        s.get("/api/status").send().await.json()["overall"],
        "degraded"
    );
    // ... the second in a row makes it an outage
    check(&s, id).await;
    assert_eq!(status_of(&s, id).await, "down");
    assert_eq!(
        s.get("/api/status").send().await.json()["overall"],
        "major_outage"
    );

    healthy.store(true, Ordering::SeqCst);
    check(&s, id).await;
    assert_eq!(status_of(&s, id).await, "up");

    // ok, fail, fail, ok = 2 of 4 = 50 %
    let monitor = &s.get("/api/status").send().await.json()["monitors"][0];
    assert_eq!(monitor["checks"], 4);
    assert_eq!(monitor["uptime_percent"], 50.0);

    let history = s
        .get(&format!("/api/monitors/{id}/history"))
        .header("x-api-key", KEY)
        .send()
        .await
        .json();
    let details: Vec<&str> = history["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["detail"].as_str().unwrap())
        .collect();
    assert_eq!(details, ["HTTP 200", "HTTP 503", "HTTP 503", "HTTP 200"]);
}

#[tokio::test]
async fn public_status_hides_targets_and_admin_list_shows_them() {
    let (target, _) = flaky_target().await;
    let s = status_app(config()).await;
    let url = format!("http://{}/health", target.addr);
    add_ok(&s, "Secret internal API", &url).await;

    let public = s.get("/api/status").send().await.text();
    assert!(public.contains("Secret internal API"));
    assert!(!public.contains(&url) && !public.contains("127.0.0.1"));
    let page = s.get("/").send().await.text();
    assert!(!page.contains("127.0.0.1"));

    let admin = s
        .get("/api/monitors")
        .header("x-api-key", KEY)
        .send()
        .await
        .json();
    assert_eq!(admin["items"][0]["target"], url);
    assert_eq!(admin["items"][0]["interval_secs"], 30);
}

#[tokio::test]
async fn tcp_checks_and_failure_modes() {
    let s = status_app(config()).await;

    // reachable
    let open = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let open_port = open.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let _ = open.accept().await;
        }
    });
    let id = add_ok(&s, "db", &format!("tcp://127.0.0.1:{open_port}")).await;
    let ok = check(&s, id).await.json();
    assert_eq!(
        (ok["ok"].as_bool(), ok["detail"].as_str()),
        (Some(true), Some("connected"))
    );

    // closed port
    let closed_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let id = add_ok(&s, "gone", &format!("tcp://127.0.0.1:{closed_port}")).await;
    let refused = check(&s, id).await.json();
    assert_eq!(
        (refused["ok"].as_bool(), refused["detail"].as_str()),
        (Some(false), Some("connection failed"))
    );

    // accepts, then never answers HTTP
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            if let Ok((sock, _)) = silent.accept().await {
                held.push(sock);
            }
        }
    });
    let id = add_ok(&s, "hung", &format!("http://127.0.0.1:{silent_port}/")).await;
    let t = Instant::now();
    let timed_out = check(&s, id).await.json();
    assert_eq!(timed_out["detail"], "timeout");
    assert!(t.elapsed() < Duration::from_secs(5));

    // answers with garbage
    let junk = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let junk_port = junk.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            if let Ok((mut sock, _)) = junk.accept().await {
                let _ = sock.write_all(b"this is not http\r\n\r\n").await;
            }
        }
    });
    let id = add_ok(&s, "junk", &format!("http://127.0.0.1:{junk_port}/")).await;
    let garbage = check(&s, id).await.json();
    assert_eq!(garbage["ok"], false);
    assert!(garbage["detail"].as_str().unwrap().contains("HTTP"));
}

#[tokio::test]
async fn the_background_scheduler_checks_on_its_own_and_stops_after_delete() {
    let (target, _) = flaky_target().await;
    let s = status_app(statuspage::Config {
        tick: Duration::from_millis(10),
        interval_unit: Duration::from_millis(20), // "1 second" = 20 ms
        ..config()
    })
    .await;
    let id = add(&s, json!({ "name": "auto", "target": format!("http://{}/health", target.addr), "interval_secs": 1 }))
        .await
        .json()["id"]
        .as_u64()
        .unwrap();

    let checks = |s: Server| async move {
        s.get("/api/status").send().await.json()["monitors"][0]["checks"]
            .as_u64()
            .unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while checks(s).await < 3 {
        assert!(
            Instant::now() < deadline,
            "scheduler never reached 3 checks"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status_of(&s, id).await, "up");

    assert_eq!(
        s.delete(&format!("/api/monitors/{id}"))
            .header("x-api-key", KEY)
            .send()
            .await
            .status,
        204
    );
    assert_eq!(
        s.get(&format!("/api/monitors/{id}/history"))
            .header("x-api-key", KEY)
            .send()
            .await
            .status,
        404
    );
    tokio::time::sleep(Duration::from_millis(100)).await; // a late result must not resurrect it
    assert_eq!(
        s.get("/api/status").send().await.json()["monitors"],
        json!([])
    );
}

#[tokio::test]
async fn unknown_ids_and_limits() {
    let s = status_app(config()).await;
    for (method, path) in [
        ("DELETE", "/api/monitors/99"),
        ("POST", "/api/monitors/99/check"),
        ("GET", "/api/monitors/99/history"),
    ] {
        assert_eq!(
            s.request(method, path)
                .header("x-api-key", KEY)
                .send()
                .await
                .status,
            404,
            "{method} {path}"
        );
    }
    assert_eq!(
        s.delete("/api/monitors/abc")
            .header("x-api-key", KEY)
            .send()
            .await
            .status,
        400
    );

    for i in 0..50 {
        add_ok(&s, &format!("m{i}"), "tcp://127.0.0.1:9").await;
    }
    let over = add(
        &s,
        json!({ "name": "one too many", "target": "tcp://127.0.0.1:9" }),
    )
    .await;
    assert_eq!(over.status, 409);
}

#[tokio::test]
async fn status_page_escapes_names_and_sets_a_strict_csp() {
    let s = status_app(config()).await;
    add_ok(&s, "<img src=x onerror=alert(1)>", "tcp://127.0.0.1:9").await;

    let page = s.get("/").send().await;
    let html = page.text();
    assert!(
        html.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "{html}"
    );
    assert!(!html.contains("<img src=x"));
    let csp = page.header("content-security-policy");
    assert!(csp.contains("default-src 'none'") && csp.contains("script-src 'self'"));
    let script_src = csp
        .split(';')
        .map(str::trim)
        .find(|d| d.starts_with("script-src"))
        .unwrap();
    assert_eq!(script_src, "script-src 'self'");
    assert!(html.contains(r#"<script src="/app.js">"#));
    assert_eq!(page.header("x-content-type-options"), "nosniff");

    let js = s.get("/app.js").send().await;
    assert_eq!(
        js.header("content-type"),
        "application/javascript; charset=utf-8"
    );
    assert!(js.text().contains("EventSource"));
    // the client builds the DOM with textContent only
    assert!(!js.text().contains("innerHTML"));
}

async fn open_events(addr: std::net::SocketAddr) -> Incoming {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let res = sender
        .send_request(
            hyper::Request::builder()
                .uri("/events")
                .header("host", "x")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.headers()["content-type"],
        "text/event-stream; charset=utf-8"
    );
    res.into_body()
}

/// Next `status` event's JSON payload
async fn next_status(body: &mut Incoming) -> Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("no event within 5 s")
            .expect("stream ended")
            .unwrap();
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        if let Some(payload) = text.lines().find_map(|l| l.strip_prefix("data: ")) {
            assert!(text.contains("event: status"), "{text}");
            return serde_json::from_str(payload).unwrap();
        }
    }
}

#[tokio::test]
async fn events_stream_the_snapshot_and_then_every_change() {
    let (target, healthy) = flaky_target().await;
    let s = status_app(config()).await;
    let id = add_ok(&s, "API", &format!("http://{}/health", target.addr)).await;

    let mut events = open_events(s.addr).await;
    let snapshot = next_status(&mut events).await;
    assert_eq!(snapshot["monitors"][0]["status"], "unknown");

    check(&s, id).await;
    let up = next_status(&mut events).await;
    assert_eq!(
        (up["overall"].as_str(), up["monitors"][0]["status"].as_str()),
        (Some("operational"), Some("up"))
    );

    healthy.store(false, Ordering::SeqCst);
    check(&s, id).await;
    check(&s, id).await;
    next_status(&mut events).await; // degraded
    let down = next_status(&mut events).await;
    assert_eq!(down["overall"], "major_outage");

    add_ok(&s, "second", "tcp://127.0.0.1:9").await;
    assert_eq!(
        next_status(&mut events).await["monitors"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn open_event_streams_do_not_block_graceful_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        statuspage::build(config())
            .shutdown_grace(Duration::from_secs(30))
            .serve_with_shutdown(listener, async {
                let _ = stopped.await;
            }),
    );

    let mut events = open_events(addr).await;
    next_status(&mut events).await;

    let t = Instant::now();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("shutdown hung on the open event stream")
        .unwrap()
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(5));
    // the stream ended instead of being cut mid-event
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.frame())
            .await
            .unwrap()
        {
            None => break,
            Some(Err(_)) => break,
            Some(Ok(_)) => continue,
        }
    }
}

#[tokio::test]
async fn openapi_lists_public_and_admin_operations() {
    let s = status_app(config()).await;
    let doc = s.get("/openapi.json").send().await.json();
    for path in [
        "/api/status",
        "/events",
        "/api/monitors",
        "/api/monitors/{id}",
        "/api/monitors/{id}/check",
        "/api/monitors/{id}/history",
    ] {
        assert!(doc["paths"][path].is_object(), "{path}");
    }
    assert_eq!(
        doc["paths"]["/api/monitors"]["post"]["security"],
        json!([{ "apiKey_x_api_key": [] }])
    );
    assert!(doc["paths"]["/api/status"]["get"].get("security").is_none());
}
