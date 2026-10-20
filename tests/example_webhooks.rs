//! The webhook dispatcher delivering to real (local) receivers

mod common;

#[allow(dead_code)]
#[path = "../examples/webhooks/app.rs"]
mod webhooks;

use common::{spawn, Server};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use ullun::jobs::QueueConfig;
use ullun::prelude::*;

const KEY: &str = "test-webhooks-key-0123456789";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn config() -> webhooks::Config {
    webhooks::Config {
        api_key: KEY.into(),
        allow_private_targets: true,
        workers: 2,
        delivery_timeout: Duration::from_millis(300),
        disable_after: 2,
        max_payload_bytes: 4096,
        data_dir: None,
        queue: QueueConfig::new()
            .base_delay(Duration::from_millis(20))
            .max_delay(Duration::from_millis(100))
            .jitter(false),
        max_attempts: 4,
        clock: Arc::new(now),
    }
}

async fn dispatcher(cfg: webhooks::Config) -> Server {
    spawn(webhooks::build(cfg)).await
}

#[derive(Clone, Debug)]
struct Received {
    headers: HashMap<String, String>,
    body: String,
    at: Instant,
}

struct Receiver {
    server: Server,
    got: Arc<Mutex<Vec<Received>>>,
}

impl Receiver {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.server.addr, path)
    }

    fn count(&self) -> usize {
        self.got.lock().unwrap().len()
    }

    fn all(&self) -> Vec<Received> {
        self.got.lock().unwrap().clone()
    }
}

/// A receiver whose n-th request (0-based) is answered by `respond(n)`:
/// (status, optional retry-after, delay before answering)
fn receiver_app(
    got: Arc<Mutex<Vec<Received>>>,
    respond: impl Fn(usize) -> (u16, Option<&'static str>, Duration) + Send + Sync + 'static,
) -> App {
    let respond = Arc::new(respond);
    App::new().post("/hook", move |req: Request| {
        let (got, respond) = (got.clone(), respond.clone());
        async move {
            let headers = req
                .headers
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|v| (k.as_str().to_string(), v.to_string()))
                })
                .collect();
            let n = {
                let mut g = got.lock().unwrap();
                g.push(Received {
                    headers,
                    body: String::from_utf8_lossy(&req.body).into_owned(),
                    at: Instant::now(),
                });
                g.len() - 1
            };
            let (status, retry_after, delay) = respond(n);
            tokio::time::sleep(delay).await;
            let mut res =
                Response::text("ok").with_status(http::StatusCode::from_u16(status).unwrap());
            if let Some(ra) = retry_after {
                res = res.with_header("retry-after", ra);
            }
            Ok(res)
        }
    })
}

async fn receiver(
    respond: impl Fn(usize) -> (u16, Option<&'static str>, Duration) + Send + Sync + 'static,
) -> Receiver {
    let got = Arc::new(Mutex::new(Vec::new()));
    let server = spawn(receiver_app(got.clone(), respond)).await;
    Receiver { server, got }
}

async fn healthy() -> Receiver {
    receiver(|_| (200, None, Duration::ZERO)).await
}

async fn register(d: &Server, url: &str, events: Value) -> Value {
    let res = d
        .post("/api/endpoints")
        .header("x-api-key", KEY)
        .json(&json!({ "url": url, "events": events }))
        .send()
        .await;
    assert_eq!(res.status, 201, "{}", res.text());
    res.json()
}

async fn publish(d: &Server, event_type: &str, data: Value) -> common::Reply {
    d.post("/api/events")
        .header("x-api-key", KEY)
        .json(&json!({ "type": event_type, "data": data }))
        .send()
        .await
}

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn api(d: &Server, method: &str, path: &str) -> common::Call {
    d.request(method, path).header("x-api-key", KEY)
}

async fn queue(d: &Server) -> Value {
    api(d, "GET", "/api/queue").send().await.json()
}

#[tokio::test]
async fn the_management_api_needs_the_key() {
    let d = dispatcher(config()).await;
    for (method, path) in [
        ("GET", "/api/endpoints"),
        ("POST", "/api/endpoints"),
        ("POST", "/api/events"),
        ("GET", "/api/queue"),
        ("GET", "/api/queue/dead"),
        ("DELETE", "/api/endpoints/x"),
    ] {
        assert_eq!(
            d.request(method, path).send().await.status,
            401,
            "{method} {path}"
        );
        assert_eq!(
            d.request(method, path)
                .header("x-api-key", "nope")
                .send()
                .await
                .status,
            401
        );
    }
    assert_eq!(d.get("/healthz").send().await.status, 200);
}

#[tokio::test]
async fn endpoint_registration_is_validated() {
    let d = dispatcher(config()).await;
    let bad = |url: &str, events: Value| {
        let call = d
            .post("/api/endpoints")
            .header("x-api-key", KEY)
            .json(&json!({ "url": url, "events": events }));
        async move { call.send().await }
    };
    for (url, events, field) in [
        ("https://example.com/hook", json!(["a"]), "url"),
        ("ftp://example.com/x", json!(["a"]), "url"),
        ("not a url", json!(["a"]), "url"),
        ("/relative", json!(["a"]), "url"),
        ("http://127.0.0.1:9/hook", json!([]), "events"),
        ("http://127.0.0.1:9/hook", json!(["UPPER"]), "events"),
        ("http://127.0.0.1:9/hook", json!(["has space"]), "events"),
        ("http://127.0.0.1:9/hook", json!(["x".repeat(51)]), "events"),
    ] {
        let res = bad(url, events.clone()).await;
        assert_eq!(res.status, 422, "{url} {events}");
        assert!(
            res.json()["fields"][field].is_array(),
            "{url} {events}: {}",
            res.text()
        );
    }
    assert_eq!(
        d.post("/api/endpoints")
            .header("x-api-key", KEY)
            .text("nope")
            .send()
            .await
            .status,
        400
    );

    // targets on private networks are refused unless explicitly allowed
    let strict = dispatcher(webhooks::Config {
        allow_private_targets: false,
        ..config()
    })
    .await;
    for url in [
        "http://127.0.0.1:9/hook",
        "http://localhost/hook",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5/hook",
        "http://[::1]/hook",
        "http://192.168.1.1:8080/",
    ] {
        let res = strict
            .post("/api/endpoints")
            .header("x-api-key", KEY)
            .json(&json!({ "url": url, "events": ["a"] }))
            .send()
            .await;
        assert_eq!(res.status, 422, "{url}");
        let msg = res.json()["fields"]["url"][0].as_str().unwrap().to_string();
        assert!(
            msg.contains("not public") || msg.contains("resolution"),
            "{url}: {msg}"
        );
    }
    let ok = strict
        .post("/api/endpoints")
        .header("x-api-key", KEY)
        .json(&json!({ "url": "http://93.184.216.34/hook", "events": ["a"] }))
        .send()
        .await;
    assert_eq!(ok.status, 201);
}

#[tokio::test]
async fn an_event_is_delivered_once_with_a_valid_signature() {
    let r = healthy().await;
    let d = dispatcher(config()).await;
    let ep = register(&d, &r.url("/hook"), json!(["order.created"])).await;
    let secret = ep["secret"].as_str().unwrap().to_string();
    assert_eq!(secret.len(), 64);
    assert!(ep["id"].as_str().unwrap().starts_with("ep_"));

    // the secret never appears again
    let listed = api(&d, "GET", "/api/endpoints").send().await.text();
    assert!(!listed.contains(&secret));
    assert!(!api(
        &d,
        "GET",
        &format!("/api/endpoints/{}", ep["id"].as_str().unwrap())
    )
    .send()
    .await
    .text()
    .contains(&secret));

    let res = publish(
        &d,
        "order.created",
        json!({ "order_id": 17, "total": "9.90" }),
    )
    .await;
    assert_eq!(res.status, 202);
    let event_id = res.json()["event_id"].as_str().unwrap().to_string();
    assert_eq!(res.json()["deliveries"], 1);

    until("delivery", || r.count() == 1).await;
    let got = r.all().remove(0);
    let h = |n: &str| got.headers.get(n).cloned().unwrap_or_default();
    assert_eq!(h("content-type"), "application/json");
    assert_eq!(h("x-webhook-id"), event_id);
    assert_eq!(h("x-webhook-event"), "order.created");
    assert_eq!(h("x-webhook-attempt"), "1");
    assert_eq!(h("user-agent"), "ullun-webhooks/1");

    let body: Value = serde_json::from_str(&got.body).unwrap();
    assert_eq!(
        (body["id"].as_str(), body["type"].as_str()),
        (Some(event_id.as_str()), Some("order.created"))
    );
    assert_eq!(body["data"], json!({ "order_id": 17, "total": "9.90" }));
    // the receiver can verify it, and tampering or a stale timestamp fails
    let sig = h("x-webhook-signature");
    let ts = h("x-webhook-timestamp");
    assert!(sig.starts_with("v1="));
    assert!(webhooks::verify_signature(
        &secret,
        &ts,
        &got.body,
        &sig,
        now(),
        300
    ));
    assert!(!webhooks::verify_signature(
        &secret,
        &ts,
        &got.body.replace("17", "18"),
        &sig,
        now(),
        300
    ));
    assert!(!webhooks::verify_signature(
        "wrong-secret",
        &ts,
        &got.body,
        &sig,
        now(),
        300
    ));
    assert!(
        !webhooks::verify_signature(&secret, &ts, &got.body, &sig, now() + 1000, 300),
        "replays outside the window are refused"
    );
    assert!(!webhooks::verify_signature(
        &secret,
        "not-a-number",
        &got.body,
        &sig,
        now(),
        300
    ));

    // log and counters
    wait_queue(&d, "completed", 1).await;
    let log = api(
        &d,
        "GET",
        &format!("/api/endpoints/{}/deliveries", ep["id"].as_str().unwrap()),
    )
    .send()
    .await
    .json();
    assert_eq!(
        (
            log["items"][0]["ok"].as_bool(),
            log["items"][0]["status"].as_u64(),
            log["items"][0]["attempt"].as_u64()
        ),
        (Some(true), Some(200), Some(1))
    );
    // nothing else arrives
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(r.count(), 1);
}

/// Wait until the queue counter `field` ("completed", "dead", ...) reaches `n`
async fn wait_queue(d: &Server, field: &str, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while queue(d).await[field].as_u64().unwrap() < n {
        assert!(Instant::now() < deadline, "queue never reached {n} {field}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn only_subscribed_enabled_endpoints_receive_an_event() {
    let (a, b, c, e) = (
        healthy().await,
        healthy().await,
        healthy().await,
        healthy().await,
    );
    let d = dispatcher(config()).await;
    register(&d, &a.url("/hook"), json!(["order.created"])).await;
    register(&d, &b.url("/hook"), json!(["*"])).await;
    register(&d, &c.url("/hook"), json!(["user.deleted"])).await;
    let off = register(&d, &e.url("/hook"), json!(["order.created"])).await;
    // switch one off by deleting nothing: use the failure path in other tests; here use a deleted endpoint
    api(
        &d,
        "DELETE",
        &format!("/api/endpoints/{}", off["id"].as_str().unwrap()),
    )
    .send()
    .await;

    let res = publish(&d, "order.created", json!({})).await;
    assert_eq!(res.json()["deliveries"], 2);
    until("both deliveries", || a.count() == 1 && b.count() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!((c.count(), e.count()), (0, 0));

    // an event nobody subscribed to is accepted and goes nowhere
    assert_eq!(
        publish(&d, "nobody.cares", json!({})).await.json()["deliveries"],
        1
    ); // only the "*" endpoint
    until("wildcard delivery", || b.count() == 2).await;
    assert_eq!(a.count(), 1);
}

#[tokio::test]
async fn failed_deliveries_are_retried_with_backoff_until_they_succeed() {
    let r = receiver(|n| {
        if n < 2 {
            (500, None, Duration::ZERO)
        } else {
            (200, None, Duration::ZERO)
        }
    })
    .await;
    let d = dispatcher(config()).await;
    let ep = register(&d, &r.url("/hook"), json!(["e"])).await;
    publish(&d, "e", json!({})).await;

    until("third attempt", || r.count() == 3).await;
    let got = r.all();
    let attempts: Vec<&str> = got
        .iter()
        .map(|g| g.headers["x-webhook-attempt"].as_str())
        .collect();
    assert_eq!(attempts, ["1", "2", "3"]);
    // exponential: ~20 ms, then ~40 ms
    assert!(got[1].at - got[0].at >= Duration::from_millis(15));
    assert!(got[2].at - got[1].at >= Duration::from_millis(35));
    // the signature is computed per attempt but covers the same body
    assert_eq!(got[0].body, got[2].body);

    wait_queue(&d, "completed", 1).await;
    let log = api(
        &d,
        "GET",
        &format!("/api/endpoints/{}/deliveries", ep["id"].as_str().unwrap()),
    )
    .send()
    .await
    .json();
    let statuses: Vec<Option<u64>> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["status"].as_u64())
        .collect();
    assert_eq!(statuses, [Some(200), Some(500), Some(500)], "newest first");
}

#[tokio::test]
async fn retry_after_is_honoured() {
    // without it the first retry would wait 5 s
    let mut cfg = config();
    cfg.queue = QueueConfig::new()
        .base_delay(Duration::from_secs(5))
        .jitter(false);
    let r = receiver(|n| {
        if n == 0 {
            (429, Some("0"), Duration::ZERO)
        } else {
            (200, None, Duration::ZERO)
        }
    })
    .await;
    let d = dispatcher(cfg).await;
    register(&d, &r.url("/hook"), json!(["e"])).await;
    let started = Instant::now();
    publish(&d, "e", json!({})).await;
    until("retry after 0 s", || r.count() == 2).await;
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn permanent_errors_are_not_retried_and_land_in_the_dead_letter_list() {
    for status in [400u16, 401, 404, 422] {
        let r = receiver(move |_| (status, None, Duration::ZERO)).await;
        let d = dispatcher(webhooks::Config {
            disable_after: 100,
            ..config()
        })
        .await;
        register(&d, &r.url("/hook"), json!(["e"])).await;
        publish(&d, "e", json!({})).await;
        wait_queue(&d, "dead", 1).await;
        assert_eq!(r.count(), 1, "HTTP {status} must not be retried");
        let dead = api(&d, "GET", "/api/queue/dead").send().await.json();
        assert_eq!(dead["items"][0]["last_error"], format!("HTTP {status}"));
        assert_eq!(dead["items"][0]["attempts"], 1);
    }
}

#[tokio::test]
async fn timeouts_and_408_429_are_retried() {
    let r = receiver(|n| match n {
        0 => (200, None, Duration::from_millis(900)), // slower than the 300 ms timeout
        1 => (408, None, Duration::ZERO),
        2 => (429, None, Duration::ZERO),
        _ => (200, None, Duration::ZERO),
    })
    .await;
    let d = dispatcher(webhooks::Config {
        max_attempts: 6,
        ..config()
    })
    .await;
    let ep = register(&d, &r.url("/hook"), json!(["e"])).await;
    publish(&d, "e", json!({})).await;
    until("fourth attempt", || r.count() == 4).await;

    let mut log;
    loop {
        log = api(
            &d,
            "GET",
            &format!("/api/endpoints/{}/deliveries", ep["id"].as_str().unwrap()),
        )
        .send()
        .await
        .json();
        if log["items"].as_array().unwrap().len() == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let errors: Vec<String> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .map(|i| i["error"].as_str().unwrap_or("-").to_string())
        .collect();
    assert_eq!(errors, ["timeout", "HTTP 408", "HTTP 429", "-"]);
}

#[tokio::test]
async fn gone_disables_the_endpoint_immediately() {
    let r = receiver(|_| (410, None, Duration::ZERO)).await;
    let d = dispatcher(config()).await;
    let ep = register(&d, &r.url("/hook"), json!(["e"])).await;
    let id = ep["id"].as_str().unwrap().to_string();
    publish(&d, "e", json!({})).await;
    wait_queue(&d, "dead", 1).await;

    let view = api(&d, "GET", &format!("/api/endpoints/{id}"))
        .send()
        .await
        .json();
    assert_eq!(view["enabled"], false);
    assert!(view["disabled_reason"].as_str().unwrap().contains("410"));
    assert_eq!(r.count(), 1);
    // events are no longer queued for it
    assert_eq!(publish(&d, "e", json!({})).await.json()["deliveries"], 0);
}

#[tokio::test]
async fn endpoints_that_keep_failing_are_switched_off_and_can_be_revived() {
    let ok = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ok.clone();
    let r = receiver(move |_| {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            (200, None, Duration::ZERO)
        } else {
            (500, None, Duration::ZERO)
        }
    })
    .await;
    let d = dispatcher(config()).await; // 4 attempts per delivery, off after 2 dead deliveries
    let ep = register(&d, &r.url("/hook"), json!(["e"])).await;
    let id = ep["id"].as_str().unwrap().to_string();

    publish(&d, "e", json!({ "n": 1 })).await;
    publish(&d, "e", json!({ "n": 2 })).await;
    until("both deliveries exhausted", || r.count() >= 8).await;
    let mut view;
    loop {
        view = api(&d, "GET", &format!("/api/endpoints/{id}"))
            .send()
            .await
            .json();
        if view["enabled"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(view["disabled_reason"]
        .as_str()
        .unwrap()
        .contains("in a row"));
    assert_eq!(r.count(), 8, "no attempts beyond the limit");
    assert_eq!(queue(&d).await["dead"], 2);
    assert_eq!(publish(&d, "e", json!({})).await.json()["deliveries"], 0);

    // fixed receiver: switch the endpoint on and retry the dead deliveries
    ok.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        api(&d, "POST", &format!("/api/endpoints/{id}/enable"))
            .send()
            .await
            .json()["enabled"],
        true
    );
    let dead = api(&d, "GET", "/api/queue/dead").send().await.json();
    let ids: Vec<u64> = dead["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    for job in &ids {
        assert_eq!(
            api(&d, "POST", &format!("/api/queue/dead/{job}/retry"))
                .send()
                .await
                .status,
            202
        );
    }
    until("recovered deliveries", || r.count() == 10).await;
    assert_eq!(
        api(&d, "POST", "/api/queue/dead/9999/retry")
            .send()
            .await
            .status,
        404
    );
    assert_eq!(
        api(&d, "POST", "/api/queue/dead/abc/retry")
            .send()
            .await
            .status,
        400
    );
}

#[tokio::test]
async fn dead_deliveries_can_be_deleted() {
    let r = receiver(|_| (404, None, Duration::ZERO)).await;
    let d = dispatcher(webhooks::Config {
        disable_after: 100,
        ..config()
    })
    .await;
    register(&d, &r.url("/hook"), json!(["e"])).await;
    publish(&d, "e", json!({})).await;
    wait_queue(&d, "dead", 1).await;
    let dead = api(&d, "GET", "/api/queue/dead").send().await.json();
    let id = dead["items"][0]["id"].as_u64().unwrap();
    assert_eq!(
        api(&d, "DELETE", &format!("/api/queue/dead/{id}"))
            .send()
            .await
            .status,
        204
    );
    assert_eq!(
        api(&d, "DELETE", &format!("/api/queue/dead/{id}"))
            .send()
            .await
            .status,
        404
    );
    assert_eq!(queue(&d).await["dead"], 0);
}

#[tokio::test]
async fn idempotency_keys_make_publish_retries_safe() {
    let r = healthy().await;
    let d = dispatcher(config()).await;
    register(&d, &r.url("/hook"), json!(["e"])).await;

    let publish_with = |key: Option<&'static str>| {
        let mut call = d
            .post("/api/events")
            .header("x-api-key", KEY)
            .json(&json!({ "type": "e", "data": {} }));
        if let Some(k) = key {
            call = call.header("idempotency-key", k);
        }
        async move { call.send().await.json() }
    };
    let first = publish_with(Some("order-17-created")).await;
    assert_eq!(
        (first["duplicate"].as_bool(), first["deliveries"].as_u64()),
        (Some(false), Some(1))
    );
    let again = publish_with(Some("order-17-created")).await;
    assert_eq!(
        (again["duplicate"].as_bool(), again["deliveries"].as_u64()),
        (Some(true), Some(0))
    );
    assert_eq!(
        again["event_id"], first["event_id"],
        "the original event is reported"
    );
    assert_eq!(
        publish_with(Some("order-18-created")).await["duplicate"],
        false
    );
    // without a key every publish is a new event
    assert_eq!(publish_with(None).await["duplicate"], false);
    assert_eq!(publish_with(None).await["duplicate"], false);

    until("deliveries", || r.count() == 4).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(r.count(), 4);
    let too_long = d
        .post("/api/events")
        .header("x-api-key", KEY)
        .header("idempotency-key", &"k".repeat(101))
        .json(&json!({ "type": "e", "data": {} }))
        .send()
        .await;
    assert_eq!(too_long.status, 400);
}

#[tokio::test]
async fn event_input_is_validated_and_limited() {
    let d = dispatcher(config()).await;
    for bad in ["UPPER", "has space", "", "*", &"x".repeat(51)] {
        let res = publish(&d, bad, json!({})).await;
        assert_eq!(res.status, 422, "{bad:?}");
    }
    assert_eq!(
        d.post("/api/events")
            .header("x-api-key", KEY)
            .json(&json!({ "type": "e" }))
            .send()
            .await
            .status,
        400
    );
    let big = d
        .post("/api/events")
        .header("x-api-key", KEY)
        .json(&json!({ "type": "e", "data": { "blob": "x".repeat(5000) } }))
        .send()
        .await;
    assert_eq!(big.status, 413);
}

#[tokio::test]
async fn rotating_the_secret_changes_the_signature_key() {
    let r = healthy().await;
    let d = dispatcher(config()).await;
    let ep = register(&d, &r.url("/hook"), json!(["e"])).await;
    let (id, old) = (
        ep["id"].as_str().unwrap().to_string(),
        ep["secret"].as_str().unwrap().to_string(),
    );
    let new = api(&d, "POST", &format!("/api/endpoints/{id}/rotate-secret"))
        .send()
        .await
        .json()["secret"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new, old);

    publish(&d, "e", json!({})).await;
    until("delivery", || r.count() == 1).await;
    let got = r.all().remove(0);
    let (ts, sig) = (
        &got.headers["x-webhook-timestamp"],
        &got.headers["x-webhook-signature"],
    );
    assert!(webhooks::verify_signature(
        &new,
        ts,
        &got.body,
        sig,
        now(),
        300
    ));
    assert!(!webhooks::verify_signature(
        &old,
        ts,
        &got.body,
        sig,
        now(),
        300
    ));
    assert_eq!(
        api(&d, "POST", "/api/endpoints/nope/rotate-secret")
            .send()
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn deleted_endpoints_stop_receiving_and_pending_work_is_dropped() {
    let r = receiver(|_| (500, None, Duration::ZERO)).await;
    let d = dispatcher(webhooks::Config {
        disable_after: 100,
        queue: QueueConfig::new()
            .base_delay(Duration::from_millis(200))
            .jitter(false),
        ..config()
    })
    .await;
    let ep = register(&d, &r.url("/hook"), json!(["e"])).await;
    let id = ep["id"].as_str().unwrap().to_string();
    publish(&d, "e", json!({})).await;
    until("first attempt", || r.count() == 1).await;
    assert_eq!(
        api(&d, "DELETE", &format!("/api/endpoints/{id}"))
            .send()
            .await
            .status,
        204
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        r.count(),
        1,
        "the retry must notice that the endpoint is gone"
    );
    assert_eq!(
        api(&d, "GET", &format!("/api/endpoints/{id}"))
            .send()
            .await
            .status,
        404
    );
    assert_eq!(
        api(&d, "DELETE", &format!("/api/endpoints/{id}"))
            .send()
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn endpoints_and_pending_deliveries_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("ullun-webhooks-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let cfg = || webhooks::Config {
        data_dir: Some(dir.clone()),
        delivery_timeout: Duration::from_millis(100),
        ..config()
    };

    // a receiver that accepts connections at the OS level but is not serving yet
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());

    let l1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr1 = l1.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let first = tokio::spawn(webhooks::build(cfg()).serve_with_shutdown(l1, async {
        let _ = stopped.await;
    }));
    let d1 = Server { addr: addr1 };
    let ep = register(&d1, &url, json!(["e"])).await;
    let secret = ep["secret"].as_str().unwrap().to_string();
    let id = ep["id"].as_str().unwrap().to_string();
    publish(&d1, "e", json!({ "n": 1 })).await;
    tokio::time::sleep(Duration::from_millis(60)).await; // the first attempt is in flight
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("first instance did not stop")
        .unwrap()
        .unwrap();

    // second instance, same directory; now the receiver starts serving
    let got = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(receiver_app(got.clone(), |_| (200, None, Duration::ZERO)).serve(listener));
    let d2 = dispatcher(cfg()).await;
    assert_eq!(
        api(&d2, "GET", &format!("/api/endpoints/{id}"))
            .send()
            .await
            .status,
        200,
        "endpoint was persisted"
    );
    until("the interrupted delivery is made after the restart", || {
        !got.lock().unwrap().is_empty()
    })
    .await;
    let first = got.lock().unwrap()[0].clone();
    assert!(webhooks::verify_signature(
        &secret,
        &first.headers["x-webhook-timestamp"],
        &first.body,
        &first.headers["x-webhook-signature"],
        now(),
        300
    ));
    assert!(first.body.contains("\"n\":1"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn openapi_describes_the_api() {
    let d = dispatcher(config()).await;
    let doc = d.get("/openapi.json").send().await.json();
    for path in [
        "/api/endpoints",
        "/api/events",
        "/api/queue/dead/{id}/retry",
        "/api/endpoints/{id}/deliveries",
    ] {
        assert!(doc["paths"][path].is_object(), "{path}");
    }
    assert_eq!(
        doc["paths"]["/api/events"]["post"]["security"],
        json!([{ "apiKey_x_api_key": [] }])
    );
    assert_eq!(d.get("/docs").send().await.status, 200);
}
