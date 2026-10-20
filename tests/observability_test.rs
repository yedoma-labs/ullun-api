//! Request ids, access logs and metrics over real HTTP

mod common;

use common::spawn;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ullun::observability::{AccessLog, Metrics, RequestIdLayer};
use ullun::prelude::*;

fn app_routes(app: App) -> App {
    app.get("/users/{id}", |req: Request| async move {
        Ok(Response::json(serde_json::json!({
            "id": req.params.get("id")?,
            "request_id": req.request_id(),
            "matched": req.matched_path.as_deref(),
        })))
    })
    .get("/boom", |_req: Request| async {
        Err::<Response, _>(Error::internal("boom"))
    })
    .get("/slow", |_req: Request| async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(Response::text("done"))
    })
}

#[tokio::test]
async fn matched_path_is_the_route_pattern() {
    let s = spawn(app_routes(App::new())).await;
    let res = s.get("/users/42").send().await.json();
    assert_eq!(res["matched"], "/users/{id}");
    assert_eq!(res["id"], "42");
}

#[tokio::test]
async fn request_ids_are_generated_propagated_and_validated() {
    let s = spawn(app_routes(App::new().middleware(RequestIdLayer::new()))).await;

    let generated = s.get("/users/1").send().await;
    let id = generated.header("x-request-id").to_string();
    assert_eq!(id.len(), 16);
    assert_eq!(generated.json()["request_id"], id);
    assert_ne!(s.get("/users/1").send().await.header("x-request-id"), id);

    // a sane id from the caller is kept (tracing across services)
    let kept = s
        .get("/users/1")
        .header("x-request-id", "trace-abc_123.x")
        .send()
        .await;
    assert_eq!(kept.header("x-request-id"), "trace-abc_123.x");
    assert_eq!(kept.json()["request_id"], "trace-abc_123.x");

    // an insane one is replaced, not echoed back into logs and headers
    for bad in ["has space", "<script>", &"a".repeat(65)] {
        let res = s.get("/users/1").header("x-request-id", bad).send().await;
        assert_eq!(res.header("x-request-id").len(), 16, "{bad:?}");
    }
    // errors and 404s carry the header too
    assert_eq!(s.get("/boom").send().await.header("x-request-id").len(), 16);
    assert_eq!(
        s.get("/nowhere").send().await.header("x-request-id").len(),
        16
    );
}

#[tokio::test]
async fn request_id_options() {
    let s = spawn(app_routes(
        App::new().middleware(
            RequestIdLayer::new()
                .header_name("X-Correlation-Id")
                .ignore_incoming(),
        ),
    ))
    .await;
    let res = s
        .get("/users/1")
        .header("x-correlation-id", "from-client")
        .send()
        .await;
    assert_ne!(res.header("x-correlation-id"), "from-client");
    assert_eq!(res.header("x-correlation-id").len(), 16);
    assert_eq!(res.header("x-request-id"), "");
}

fn capture() -> (
    Arc<Mutex<Vec<String>>>,
    impl Fn(&str) + Send + Sync + 'static,
) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    (lines, move |line: &str| {
        sink.lock().unwrap().push(line.to_string())
    })
}

#[tokio::test]
async fn access_log_lines_have_route_status_timing_and_request_id() {
    let (lines, sink) = capture();
    let s = spawn(app_routes(
        App::new()
            .middleware(RequestIdLayer::new())
            .middleware(AccessLog::new().sink(sink).clock(|| 784_111_777)),
    ))
    .await;

    let res = s
        .get("/users/42?token=secret")
        .header("user-agent", "tester/1.0")
        .send()
        .await;
    s.get("/boom").send().await;
    s.get("/missing/path/7").send().await;

    let lines = lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 3);
    let id = res.header("x-request-id");
    assert_eq!(
        lines[0]
            .replace(&format!("id={id}"), "id=ID")
            .split(' ')
            .map(str::to_string)
            .collect::<Vec<_>>()[..3],
        ["1994-11-06T08:49:37Z", "127.0.0.1", "GET"]
    );
    assert!(lines[0].contains(" /users/42 200 "), "{}", lines[0]); // path without the query
    assert!(
        !lines[0].contains("secret"),
        "query strings stay out of the log"
    );
    assert!(lines[0].contains("route=/users/{id}") && lines[0].contains(&format!("id={id}")));
    assert!(lines[0].contains("ua=\"tester/1.0\"") && lines[0].contains("ms "));
    // errors are logged with their status, unmatched requests without a route
    assert!(
        lines[1].contains(" /boom 500 ") && lines[1].contains("route=/boom"),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].contains(" /missing/path/7 404 ") && lines[2].contains("route=- "),
        "{}",
        lines[2]
    );
}

#[tokio::test]
async fn access_log_json_and_query_opt_in() {
    let (lines, sink) = capture();
    let s = spawn(app_routes(
        App::new().middleware(AccessLog::new().json().include_query().sink(sink)),
    ))
    .await;
    s.get("/users/9?x=1").send().await;

    let line = lines.lock().unwrap()[0].clone();
    let j: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(j["path"], "/users/9?x=1");
    assert_eq!(j["route"], "/users/{id}");
    assert_eq!(
        (j["status"].as_u64(), j["method"].as_str()),
        (Some(200), Some("GET"))
    );
    assert!(j["bytes"].as_u64().unwrap() > 0);
    assert!(j["request_id"].is_null(), "no RequestIdLayer installed");
    assert!(j["time"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn streamed_responses_log_no_byte_count() {
    let (lines, sink) = capture();
    let s = spawn(App::new().middleware(AccessLog::new().sink(sink)).get(
        "/s",
        |_req: Request| async {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                let _ = tx.send("x").await;
            });
            Ok(res)
        },
    ))
    .await;
    s.get("/s").send().await;
    assert!(
        lines.lock().unwrap()[0].contains(" 200 - "),
        "{}",
        lines.lock().unwrap()[0]
    );
}

#[tokio::test]
async fn metrics_count_by_route_pattern_not_by_path() {
    let metrics = Metrics::new();
    let s = spawn(app_routes(
        App::new()
            .middleware(metrics.clone())
            .get("/metrics", metrics.handler()),
    ))
    .await;

    for id in 1..=20 {
        s.get(&format!("/users/{id}")).send().await;
    }
    s.get("/boom").send().await;
    for scan in ["/.env", "/wp-login.php", "/admin/../etc/passwd"] {
        s.get(scan).send().await;
    }
    s.request("HEAD", "/users/1").send().await;
    s.post("/users/1").send().await; // 405

    let res = s.get("/metrics").send().await;
    assert_eq!(
        res.header("content-type"),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = res.text();
    let line = |needle: &str| {
        text.lines()
            .find(|l| l.starts_with(needle))
            .unwrap_or_else(|| panic!("{needle} in\n{text}"))
            .to_string()
    };

    assert!(
        line("http_requests_total{method=\"GET\",route=\"/users/{id}\",status=\"200\"}")
            .ends_with(" 20")
    );
    assert!(
        line("http_requests_total{method=\"HEAD\",route=\"/users/{id}\",status=\"200\"}")
            .ends_with(" 1")
    );
    assert!(
        line("http_requests_total{method=\"GET\",route=\"/boom\",status=\"500\"}").ends_with(" 1")
    );
    // everything unmatched shares one series, however many paths were probed
    assert!(
        line("http_requests_total{method=\"GET\",route=\"unmatched\",status=\"404\"}")
            .ends_with(" 3")
    );
    assert!(
        line("http_requests_total{method=\"POST\",route=\"unmatched\",status=\"405\"}")
            .ends_with(" 1")
    );
    assert!(
        !text.contains("/users/7") && !text.contains(".env"),
        "raw paths must not become labels"
    );
    // histogram is complete
    assert!(
        line("http_request_duration_seconds_count{method=\"GET\",route=\"/users/{id}\"}")
            .ends_with(" 20")
    );
    assert!(text.contains(
        "http_request_duration_seconds_bucket{method=\"GET\",route=\"/users/{id}\",le=\"+Inf\"} 20"
    ));
}

#[tokio::test]
async fn in_flight_gauge_tracks_running_requests() {
    let metrics = Metrics::new();
    let s = spawn(app_routes(
        App::new()
            .middleware(metrics.clone())
            .get("/metrics", metrics.handler()),
    ))
    .await;
    let gauge = |m: &Metrics| {
        m.render()
            .lines()
            .find(|l| l.starts_with("http_requests_in_flight "))
            .and_then(|l| l.rsplit(' ').next().map(str::to_string))
            .unwrap()
    };

    assert_eq!(gauge(&metrics), "0");
    let slow = tokio::spawn(async move { s.get("/slow").send().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gauge(&metrics), "1");
    slow.await.unwrap();
    assert_eq!(gauge(&metrics), "0");
}

#[tokio::test]
async fn in_flight_is_released_when_the_client_disconnects() {
    use http_body_util::Empty;
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;

    let metrics = Metrics::new();
    let s = spawn(app_routes(App::new().middleware(metrics.clone()))).await;
    let stream = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let conn_task = tokio::spawn(conn);
    let pending = tokio::spawn(async move {
        let req = hyper::Request::builder()
            .uri("/slow")
            .header("host", "x")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let _ = sender.send_request(req).await;
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(metrics.render().contains("http_requests_in_flight 1\n"));
    // hang up mid-request
    pending.abort();
    conn_task.abort();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !metrics.render().contains("http_requests_in_flight 0\n") {
        assert!(
            std::time::Instant::now() < deadline,
            "gauge leaked after a disconnect"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
