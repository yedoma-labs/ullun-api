//! W3C trace context propagation

mod common;

use common::spawn;
use std::sync::{Arc, Mutex};
use ullun::client::HttpClient;
use ullun::observability::AccessLog;
use ullun::prelude::*;
use ullun::trace::{TraceContext, TraceLayer};

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00f067aa0ba902b7";

fn echo_app(layer: TraceLayer) -> App {
    App::new()
        .middleware(layer)
        .get("/trace", |req: Request| async move {
            let t = req.trace().unwrap();
            Ok(Response::json(serde_json::json!({
                "trace_id": t.trace_id, "span_id": t.span_id,
                "parent": t.parent_span_id, "sampled": t.sampled, "state": t.tracestate,
            })))
        })
}

#[tokio::test]
async fn continues_an_incoming_trace_with_a_new_span() {
    let s = spawn(echo_app(TraceLayer::new())).await;
    let res = s
        .get("/trace")
        .header("traceparent", &format!("00-{TRACE}-{PARENT}-01"))
        .header("tracestate", "vendor=abc")
        .send()
        .await;
    let t = res.json();
    assert_eq!(t["trace_id"], TRACE);
    assert_eq!(t["parent"], PARENT);
    assert_ne!(t["span_id"], PARENT);
    assert_eq!(t["span_id"].as_str().unwrap().len(), 16);
    assert_eq!(t["sampled"], true);
    assert_eq!(t["state"], "vendor=abc");
    assert!(!res.headers.contains_key("traceparent"));
}

#[tokio::test]
async fn starts_a_trace_without_or_with_a_bad_header() {
    let s = spawn(echo_app(TraceLayer::new())).await;
    for header in [None, Some("garbage"), Some("00-0000-00-01")] {
        let mut call = s.get("/trace");
        if let Some(h) = header {
            call = call.header("traceparent", h);
        }
        let t = call.send().await.json();
        assert_eq!(t["trace_id"].as_str().unwrap().len(), 32, "{header:?}");
        assert!(t["parent"].is_null());
        assert!(t["state"].is_null());
    }
    let a = s.get("/trace").send().await.json();
    let b = s.get("/trace").send().await.json();
    assert_ne!(a["trace_id"], b["trace_id"]);
}

#[tokio::test]
async fn unsampled_flag_and_ignore_incoming() {
    let s = spawn(echo_app(TraceLayer::new())).await;
    let t = s
        .get("/trace")
        .header("traceparent", &format!("00-{TRACE}-{PARENT}-00"))
        .send()
        .await
        .json();
    assert_eq!(t["sampled"], false);

    let edge = spawn(echo_app(TraceLayer::new().ignore_incoming())).await;
    let t = edge
        .get("/trace")
        .header("traceparent", &format!("00-{TRACE}-{PARENT}-01"))
        .header("tracestate", "x=1")
        .send()
        .await
        .json();
    assert_ne!(t["trace_id"], TRACE);
    assert!(t["parent"].is_null());
    assert!(t["state"].is_null());
}

#[tokio::test]
async fn response_can_echo_the_servers_span() {
    let s = spawn(echo_app(TraceLayer::new().echo_response())).await;
    let res = s
        .get("/trace")
        .header("traceparent", &format!("00-{TRACE}-{PARENT}-01"))
        .send()
        .await;
    let (trace, span, _) = TraceContext::parse(res.header("traceparent")).unwrap();
    assert_eq!(trace, TRACE);
    assert_eq!(span, res.json()["span_id"].as_str().unwrap());
}

#[tokio::test]
async fn outgoing_calls_carry_the_trace() {
    let downstream = spawn(echo_app(TraceLayer::new())).await;
    let target = format!("http://{}/trace", downstream.addr);
    let front = App::new()
        .middleware(TraceLayer::new())
        .get("/go", move |req: Request| {
            let target = target.clone();
            async move {
                let res = HttpClient::new().get(&target).trace(&req).send().await?;
                Ok(Response::json(serde_json::json!({
                    "mine": req.trace().map(|t| t.span_id.clone()),
                    "downstream": res.json::<serde_json::Value>()?,
                })))
            }
        });
    let s = spawn(front).await;
    let body = s
        .get("/go")
        .header("traceparent", &format!("00-{TRACE}-{PARENT}-01"))
        .header("tracestate", "vendor=abc")
        .send()
        .await
        .json();
    let d = &body["downstream"];
    assert_eq!(d["trace_id"], TRACE);
    // the downstream span's parent is the span of the front service
    assert_eq!(d["parent"], body["mine"]);
    assert_eq!(d["state"], "vendor=abc");
}

#[tokio::test]
async fn trace_without_the_layer_sends_nothing() {
    let downstream = spawn(App::new().get("/h", |req: Request| async move {
        Ok(Response::text(
            req.headers.contains_key("traceparent").to_string(),
        ))
    }))
    .await;
    let target = format!("http://{}/h", downstream.addr);
    let front = App::new().get("/go", move |req: Request| {
        let target = target.clone();
        async move {
            let res = HttpClient::new().get(&target).trace(&req).send().await?;
            Ok(Response::text(res.text()?))
        }
    });
    let s = spawn(front).await;
    assert_eq!(s.get("/go").send().await.text(), "false");
}

#[tokio::test]
async fn access_log_includes_the_trace_id() {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let app = echo_app(TraceLayer::new()).middleware(
        AccessLog::new()
            .sink(move |l| sink.lock().unwrap().push(l.to_string()))
            .clock(|| 0),
    );
    // AccessLog must run inside the trace layer to see the ids
    let s = spawn(app).await;
    s.get("/trace")
        .header("traceparent", &format!("00-{TRACE}-{PARENT}-01"))
        .send()
        .await;
    let line = lines.lock().unwrap()[0].clone();
    assert!(line.ends_with(&format!("trace={TRACE}")), "{line}");

    let json_lines = Arc::new(Mutex::new(Vec::new()));
    let sink = json_lines.clone();
    let app = echo_app(TraceLayer::new()).middleware(
        AccessLog::new()
            .json()
            .sink(move |l| sink.lock().unwrap().push(l.to_string())),
    );
    let s = spawn(app).await;
    s.get("/trace")
        .header("traceparent", &format!("00-{TRACE}-{PARENT}-01"))
        .send()
        .await;
    let j: serde_json::Value = serde_json::from_str(&json_lines.lock().unwrap()[0]).unwrap();
    assert_eq!(j["trace_id"], TRACE);
}
