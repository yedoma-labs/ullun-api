//! The gateway example in front of real (local) upstream servers

mod common;

#[allow(dead_code)]
#[path = "../examples/gateway/app.rs"]
mod gateway;

use common::{spawn, Server};
use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use ullun::prelude::*;

const ADMIN: &str = "test-admin-key-0123456789";

fn config(routes: Vec<(&str, Vec<String>)>) -> gateway::Config {
    gateway::Config {
        routes: routes
            .into_iter()
            .map(|(prefix, upstreams)| gateway::RouteConfig {
                prefix: prefix.into(),
                upstreams,
                strip_prefix: true,
            })
            .collect(),
        admin_key: ADMIN.into(),
        upstream_timeout: Duration::from_millis(300),
        retries: 1,
        failure_threshold: 3,
        cooldown: Duration::from_millis(150),
        client_rate_limit: (10_000, Duration::from_secs(60)),
        max_request_body: 64 * 1024,
        log_requests: false,
    }
}

fn url(s: &Server) -> String {
    format!("http://{}", s.addr)
}

/// An address nothing listens on
async fn dead_upstream() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}")
}

/// Upstream that reports what it received
async fn echo(name: &'static str) -> Server {
    let handler = move |req: Request| async move {
        let h = |n: &str| {
            req.headers
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let uri = match req.uri.query() {
            Some(q) => format!("{}?{}", req.uri.path(), q),
            None => req.uri.path().to_string(),
        };
        Ok(Response::json(json!({
            "who": name,
            "method": req.method.as_str(),
            "uri": uri,
            "body": String::from_utf8_lossy(&req.body),
            "host": h("host"),
            "xff": h("x-forwarded-for"),
            "xfh": h("x-forwarded-host"),
            "xfp": h("x-forwarded-proto"),
            "via": h("via"),
            "request_id": h("x-request-id"),
            "connection": h("connection"),
            "secret": h("x-secret"),
            "custom": h("x-custom"),
        }))
        .with_header("x-seen-method", req.method.as_str())
        .with_header("x-hop", "should not reach the client")
        .with_header("connection", "x-hop")
        .with_header("keep-alive", "timeout=5"))
    };
    let mut app = App::new();
    for p in ["/{*any}", "/"] {
        app = app
            .get(p, handler)
            .post(p, handler)
            .put(p, handler)
            .patch(p, handler)
            .delete(p, handler);
    }
    spawn(app).await
}

async fn gw(routes: Vec<(&str, Vec<String>)>) -> Server {
    spawn(gateway::build(config(routes))).await
}

#[tokio::test]
async fn requests_are_routed_by_prefix_with_method_path_query_and_body() {
    let users = echo("users").await;
    let orders = echo("orders").await;
    let g = gw(vec![
        ("/api/users", vec![url(&users)]),
        ("/api/orders", vec![url(&orders)]),
    ])
    .await;

    let r = g
        .get("/api/users/42/profile?lang=de&x=a%20b")
        .send()
        .await
        .json();
    assert_eq!(
        (r["who"].as_str(), r["method"].as_str()),
        (Some("users"), Some("GET"))
    );
    assert_eq!(r["uri"], "/42/profile?lang=de&x=a%20b"); // prefix stripped, query untouched

    let r = g
        .post("/api/orders/new")
        .json(&json!({ "item": "book" }))
        .send()
        .await
        .json();
    assert_eq!(
        (r["who"].as_str(), r["method"].as_str()),
        (Some("orders"), Some("POST"))
    );
    assert_eq!(r["body"], r#"{"item":"book"}"#);

    for method in ["PUT", "PATCH", "DELETE"] {
        let r = g
            .request(method, "/api/users/7")
            .text("payload")
            .send()
            .await
            .json();
        assert_eq!(
            (r["method"].as_str(), r["uri"].as_str()),
            (Some(method), Some("/7")),
            "{method}"
        );
    }
    // the prefix itself maps to "/"
    assert_eq!(g.get("/api/users").send().await.json()["uri"], "/");
    assert_eq!(g.get("/api/users/").send().await.json()["uri"], "/");
    assert_eq!(g.get("/api/users/?a=1").send().await.json()["uri"], "/?a=1");
    // HEAD reaches the upstream as HEAD and has no body
    let head = g.request("HEAD", "/api/users/1").send().await;
    assert_eq!(
        (head.status, head.body.len(), head.header("x-seen-method")),
        (200, 0, "HEAD")
    );
    // unknown routes
    assert_eq!(g.get("/nope").send().await.status, 404);
    assert_eq!(g.get("/api/usersX/1").send().await.status, 404);
}

#[tokio::test]
async fn forwarded_headers_are_set_by_the_gateway_not_the_client() {
    let up = echo("up").await;
    let g = gw(vec![("/svc", vec![url(&up)])]).await;

    let r = g
        .get("/svc/x")
        .header("x-forwarded-for", "6.6.6.6")
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("via", "1.0 fake")
        .header("x-custom", "kept")
        .header("x-request-id", "client-chosen")
        .send()
        .await;
    let seen = r.json();
    assert_eq!(
        seen["xff"], "127.0.0.1",
        "spoofed X-Forwarded-For is replaced"
    );
    assert_eq!(seen["xfh"], "localhost");
    assert_eq!(seen["xfp"], "http");
    assert_eq!(seen["via"], "1.1 ullun-gateway");
    assert_eq!(seen["custom"], "kept");
    assert_eq!(
        seen["host"],
        format!("127.0.0.1:{}", up.addr.port()),
        "Host is the upstream's"
    );
    // the id is the gateway's own and travels both ways
    let id = r.header("x-request-id").to_string();
    assert_eq!(id.len(), 16);
    assert_eq!(seen["request_id"], id);
    assert_ne!(id, "client-chosen");
}

#[tokio::test]
async fn hop_by_hop_headers_do_not_cross_the_gateway() {
    let up = echo("up").await;
    let g = gw(vec![("/svc", vec![url(&up)])]).await;

    // request direction: headers named by Connection are for this hop only
    let r = g
        .get("/svc/x")
        .header("connection", "x-secret")
        .header("x-secret", "1")
        .send()
        .await
        .json();
    assert_eq!(r["secret"], Value::Null);
    assert_ne!(r["connection"], "x-secret");

    // response direction
    let res = g.get("/svc/x").send().await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("x-hop"), "");
    assert_eq!(res.header("keep-alive"), "");
    assert_eq!(res.header("via"), "1.1 ullun-gateway");
    assert_eq!(res.header("content-type"), "application/json");

    // protocol upgrades are not tunnelled
    let ws = g
        .get("/svc/socket")
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .send()
        .await;
    assert_eq!(ws.status, 501);
}

#[tokio::test]
async fn paths_cannot_escape_their_route() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let handler = move |_req: Request| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Response::text("reached"))
        }
    };
    let up = spawn(App::new().get("/{*any}", handler.clone()).get("/", handler)).await;
    let g = gw(vec![("/api/users", vec![url(&up)])]).await;

    for bad in [
        "/api/users/../admin",
        "/api/users/./x",
        "/api/users/%2e%2e/admin",
        "/api/users/%2E%2E%2Fadmin",
        "/api/users/%252e%252e/admin",
        "/api/users/a/%2e%2e/%2e%2e/admin",
        "/api//users",
        "/api/users//x",
        "/api/users/a%5Cb",
        "/api/users/a%00b",
        "/api/users/a%0d%0ab",
    ] {
        let status = g.get(bad).send().await.status;
        // refused by the gateway (400) or not routed at all (404), never forwarded
        assert!(status == 400 || status == 404, "{bad}: {status}");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "a suspicious path reached the upstream"
    );

    let mut fine = 0;
    for ok in [
        "/api/users/a..b",
        "/api/users/..hidden",
        "/api/users/v1.2/x",
        "/api/users/%7Euser",
        "/api/users/caf%C3%A9",
    ] {
        assert_eq!(g.get(ok).send().await.status, 200, "{ok}");
        fine += 1;
    }
    assert_eq!(hits.load(Ordering::SeqCst), fine);
}

#[tokio::test]
async fn load_is_spread_round_robin() {
    let (a, b, c) = (echo("a").await, echo("b").await, echo("c").await);
    let g = gw(vec![("/svc", vec![url(&a), url(&b), url(&c)])]).await;
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(
            g.get("/svc/x").send().await.json()["who"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(seen, ["a", "b", "c", "a", "b", "c"]);
}

#[tokio::test]
async fn idempotent_requests_fail_over_but_posts_are_not_replayed() {
    let live = echo("live").await;
    let g = gw(vec![("/svc", vec![dead_upstream().await, url(&live)])]).await;

    // first pick is the dead upstream; a GET is retried on the live one
    let res = g.get("/svc/x").send().await;
    assert_eq!(
        (res.status, res.json()["who"].as_str().map(str::to_string)),
        (200, Some("live".into()))
    );

    // the next request starts at the live upstream; the following at the dead one again
    assert_eq!(g.post("/svc/x").text("one").send().await.status, 200);
    let post = g.post("/svc/x").text("two").send().await;
    assert_eq!(
        post.status, 502,
        "a POST must not be sent twice just because the first attempt failed"
    );
    assert!(post.text().contains("Upstream unavailable"));
}

#[tokio::test]
async fn a_failing_upstream_is_taken_out_of_rotation_and_probed_again() {
    let healthy = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));
    let (flag, counter) = (healthy.clone(), hits.clone());
    let up =
        spawn(App::new().get("/{*any}", move |_req: Request| {
            let (flag, counter) = (flag.clone(), counter.clone());
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                if flag.load(Ordering::SeqCst) {
                    Ok(Response::text("fine"))
                } else {
                    Ok(Response::text("unavailable")
                        .with_status(http::StatusCode::SERVICE_UNAVAILABLE))
                }
            }
        }))
        .await;

    let mut cfg = config(vec![("/svc", vec![url(&up)])]);
    cfg.failure_threshold = 2;
    cfg.retries = 0;
    let g = spawn(gateway::build(cfg)).await;
    let state = |g: Server| async move {
        g.get("/_gateway/status")
            .header("x-api-key", ADMIN)
            .send()
            .await
            .json()["routes"][0]["upstreams"][0]["state"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // the upstream's own 503s are passed through and counted as failures
    assert_eq!(g.get("/svc/x").send().await.status, 503);
    assert_eq!(state(g).await, "up");
    assert_eq!(g.get("/svc/x").send().await.status, 503);
    assert_eq!(state(g).await, "down");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // while it is out, the gateway answers by itself and does not hit it
    let skipped = g.get("/svc/x").send().await;
    assert_eq!(skipped.status, 503);
    assert!(skipped.text().contains("No healthy upstream"));
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // recovered upstream + cooldown over: one request probes it, then it is back
    healthy.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(state(g).await, "probing");
    assert_eq!(g.get("/svc/x").send().await.text(), "fine");
    assert_eq!(state(g).await, "up");
    assert_eq!(g.get("/svc/x").send().await.status, 200);
}

#[tokio::test]
async fn an_abandoned_probe_does_not_wedge_the_upstream() {
    let slow = spawn(App::new().get("/{*any}", |_req: Request| async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        Ok(Response::text("late"))
    }))
    .await;
    let mut cfg = config(vec![("/svc", vec![url(&slow)])]);
    cfg.failure_threshold = 1;
    cfg.retries = 0;
    cfg.upstream_timeout = Duration::from_millis(100);
    cfg.cooldown = Duration::from_millis(100);
    let g = spawn(gateway::build(cfg)).await;

    assert_eq!(g.get("/svc/x").send().await.status, 504); // down for 100 ms
    tokio::time::sleep(Duration::from_millis(150)).await;

    // a probe starts, then the client gives up (connection dropped) before it ends
    let stream = TcpStream::connect(g.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let conn_task = tokio::spawn(conn);
    let abandoned = tokio::spawn(async move {
        let req = hyper::Request::builder()
            .uri("/svc/x")
            .header("host", "x")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let _ = sender.send_request(req).await;
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    abandoned.abort();
    conn_task.abort();
    tokio::time::sleep(Duration::from_millis(250)).await;

    // the upstream must be probe-able again (504 from the slow upstream), not stuck on 503
    let again = g.get("/svc/x").send().await;
    assert_eq!(again.status, 504, "{}", again.text());
}

#[tokio::test]
async fn slow_upstreams_time_out_with_504_and_get_retried_elsewhere() {
    let slow = spawn(
        App::new()
            .get("/{*any}", |_req: Request| async {
                tokio::time::sleep(Duration::from_millis(900)).await;
                Ok(Response::text("too late"))
            })
            .post("/{*any}", |_req: Request| async {
                tokio::time::sleep(Duration::from_millis(900)).await;
                Ok(Response::text("too late"))
            }),
    )
    .await;
    let fast = echo("fast").await;

    let alone = gw(vec![("/svc", vec![url(&slow)])]).await;
    let res = alone.get("/svc/x").send().await;
    assert_eq!(res.status, 504);
    assert!(res.text().contains("timed out"));

    // fresh gateways, so the slow upstream is picked first
    let g = gw(vec![("/svc", vec![url(&slow), url(&fast)])]).await;
    assert_eq!(g.get("/svc/x").send().await.json()["who"], "fast");
    let g = gw(vec![("/svc", vec![url(&slow), url(&fast)])]).await;
    assert_eq!(g.post("/svc/x").text("data").send().await.status, 504);
}

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
                .header("host", "x")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn chunk(body: &mut Incoming) -> Option<Bytes> {
    tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("no chunk in 5 s")?
        .unwrap()
        .into_data()
        .ok()
}

#[tokio::test]
async fn streamed_responses_are_relayed_as_they_arrive() {
    let (gate_tx, gate_rx) = oneshot::channel::<()>();
    let gate = Arc::new(tokio::sync::Mutex::new(Some(gate_rx)));
    let up = spawn(App::new().get("/stream", move |_req: Request| {
        let gate = gate.clone();
        async move {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                let _ = tx.send("first\n").await;
                if let Some(g) = gate.lock().await.take() {
                    let _ = g.await;
                }
                let _ = tx.send("second\n").await;
            });
            Ok(res.with_header("content-type", "text/plain"))
        }
    }))
    .await;
    let g = gw(vec![("/svc", vec![url(&up)])]).await;

    let res = open(g.addr, "/svc/stream").await;
    assert_eq!(res.status(), 200);
    let mut body = res.into_body();
    // the first chunk arrives while the upstream is still holding back the rest
    assert_eq!(chunk(&mut body).await.unwrap(), "first\n");
    gate_tx.send(()).unwrap();
    assert_eq!(chunk(&mut body).await.unwrap(), "second\n");
    assert!(chunk(&mut body).await.is_none());
}

#[tokio::test]
async fn a_client_that_hangs_up_cancels_the_upstream_stream() {
    let (seen_tx, seen_rx) = oneshot::channel::<()>();
    let seen = Arc::new(tokio::sync::Mutex::new(Some(seen_tx)));
    let up = spawn(App::new().get("/endless", move |_req: Request| {
        let seen = seen.clone();
        async move {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                loop {
                    if tx.send("tick\n").await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if let Some(s) = seen.lock().await.take() {
                    let _ = s.send(());
                }
            });
            Ok(res)
        }
    }))
    .await;
    let g = gw(vec![("/svc", vec![url(&up)])]).await;

    let mut body = open(g.addr, "/svc/endless").await.into_body();
    assert_eq!(chunk(&mut body).await.unwrap(), "tick\n");
    drop(body);

    tokio::time::timeout(Duration::from_secs(5), seen_rx)
        .await
        .expect("upstream kept streaming to a client that was gone")
        .unwrap();
}

#[tokio::test]
async fn large_bodies_pass_through_intact() {
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let expected = big.clone();
    let up = spawn(App::new().get("/blob", move |_req: Request| {
        let big = big.clone();
        async move {
            Ok(Response::ok()
                .with_header("content-type", "application/octet-stream")
                .with_body(big))
        }
    }))
    .await;
    let g = gw(vec![("/svc", vec![url(&up)])]).await;
    let res = g.get("/svc/blob").send().await;
    assert_eq!(res.body.len(), 3_000_000);
    assert!(res.body == expected);
}

#[tokio::test]
async fn client_side_limits_apply_before_anything_is_forwarded() {
    let up = echo("up").await;
    let mut cfg = config(vec![("/svc", vec![url(&up)])]);
    cfg.client_rate_limit = (6, Duration::from_secs(60));
    let g = spawn(gateway::build(cfg)).await;

    // rejected while reading the body: never reaches the proxy, costs no budget
    let big = g.post("/svc/x").body(vec![b'x'; 100 * 1024]).send().await;
    assert_eq!(big.status, 413);

    for i in 0..6 {
        assert_eq!(g.get("/svc/x").send().await.status, 200, "request {i}");
    }
    let limited = g.get("/svc/x").send().await;
    assert_eq!(limited.status, 429);
    assert!(limited.header("retry-after").parse::<u64>().unwrap() >= 1);
}

#[tokio::test]
async fn operator_endpoints_need_the_key_and_show_health_and_metrics() {
    let up = echo("up").await;
    let g = gw(vec![("/api/users", vec![url(&up), dead_upstream().await])]).await;
    g.get("/api/users/1").send().await;
    g.get("/api/users/2").send().await;
    g.get("/missing").send().await;

    for path in ["/_gateway/status", "/_gateway/metrics"] {
        assert_eq!(g.get(path).send().await.status, 401, "{path}");
        assert_eq!(
            g.get(path).header("x-api-key", "wrong").send().await.status,
            401
        );
    }
    let status = g
        .get("/_gateway/status")
        .header("x-api-key", ADMIN)
        .send()
        .await
        .json();
    assert_eq!(status["routes"][0]["prefix"], "/api/users");
    assert_eq!(
        status["routes"][0]["upstreams"].as_array().unwrap().len(),
        2
    );
    assert_eq!(status["routes"][0]["upstreams"][0]["url"], url(&up));

    let metrics = g
        .get("/_gateway/metrics")
        .header("x-api-key", ADMIN)
        .send()
        .await
        .text();
    // labelled by route pattern: two different ids, one series
    assert!(
        metrics.contains("route=\"/api/users/{*rest}\",status=\"200\"} 2"),
        "{metrics}"
    );
    assert!(metrics.contains("route=\"unmatched\",status=\"404\"} 1"));
    assert!(!metrics.contains("/api/users/1"));
}

#[tokio::test]
async fn config_parsing() {
    std::env::remove_var("GATEWAY_ROUTES");
    std::env::remove_var("GATEWAY_ADMIN_KEY");
    std::env::remove_var("DEV");
    assert!(gateway::Config::from_env().is_err());

    std::env::set_var(
        "GATEWAY_ROUTES",
        "/a=http://h1:1,http://h2:2;/b=http://h3:3",
    );
    assert!(
        gateway::Config::from_env().is_err(),
        "admin key is required outside DEV"
    );
    std::env::set_var("GATEWAY_ADMIN_KEY", "short");
    assert!(gateway::Config::from_env().is_err());
    std::env::set_var("GATEWAY_ADMIN_KEY", "a-long-enough-admin-key-123");
    let cfg = gateway::Config::from_env().unwrap();
    assert_eq!(cfg.routes.len(), 2);
    assert_eq!(cfg.routes[0].upstreams, ["http://h1:1", "http://h2:2"]);
    assert_eq!(cfg.routes[1].prefix, "/b");

    std::env::set_var("GATEWAY_ROUTES", "no-equals-sign");
    assert!(gateway::Config::from_env().is_err());
    std::env::remove_var("GATEWAY_ROUTES");
    std::env::remove_var("GATEWAY_ADMIN_KEY");
}

#[tokio::test]
#[should_panic(expected = "must look like http://host:port")]
async fn tls_upstreams_are_refused_at_startup() {
    let _ = gateway::build(config(vec![("/x", vec!["https://example.com".into()])]));
}
