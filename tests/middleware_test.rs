//! Middleware behaviour over real HTTP on ephemeral ports

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use ullun::prelude::*;

async fn spawn(app: App) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

async fn send(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
) -> (u16, hyper::HeaderMap, String) {
    send_with(addr, method, path, &[]).await
}

async fn send_with(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, hyper::HeaderMap, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let req = builder.body(Full::<Bytes>::default()).unwrap();
    let res = sender.send_request(req).await.unwrap();
    let (status, headers) = (res.status().as_u16(), res.headers().clone());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

async fn tag(req: Request, next: Next) -> Result<Response> {
    let res = match next.run(req).await {
        Ok(res) => res,
        Err(err) => Response::from_error(err),
    };
    Ok(res.with_header("x-tagged", "yes"))
}

#[tokio::test]
async fn cors_preflight_is_answered_without_a_matching_route() {
    let addr = spawn(
        App::new()
            .middleware(ullun::cors::Cors::new().allow_origin("https://app.example.com"))
            .get("/data", |_req: Request| async { Ok(Response::text("x")) }),
    )
    .await;

    let (status, headers, _) = send_with(
        addr,
        "OPTIONS",
        "/data",
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(
        headers["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert!(headers.contains_key("access-control-allow-methods"));
}

#[tokio::test]
async fn middleware_wraps_not_found_responses() {
    let addr = spawn(App::new().middleware(tag)).await;

    let (status, headers, body) = send(addr, "GET", "/missing").await;
    assert_eq!(status, 404);
    assert_eq!(headers["x-tagged"], "yes");
    assert!(body.contains("Route not found"));

    // unsupported methods end in the same 404
    let (status, headers, _) = send(addr, "TRACE", "/missing").await;
    assert_eq!(status, 404);
    assert_eq!(headers["x-tagged"], "yes");
}

#[tokio::test]
async fn middleware_runs_in_registration_order() {
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let (a, b) = (order.clone(), order.clone());
    let addr = spawn(
        App::new()
            .middleware(move |req: Request, next: Next| {
                let a = a.clone();
                async move {
                    a.lock().unwrap().push("a");
                    next.run(req).await
                }
            })
            .middleware(move |req: Request, next: Next| {
                let b = b.clone();
                async move {
                    b.lock().unwrap().push("b");
                    next.run(req).await
                }
            })
            .get("/", |_req: Request| async { Ok(Response::text("ok")) }),
    )
    .await;

    assert_eq!(send(addr, "GET", "/").await.0, 200);
    assert_eq!(*order.lock().unwrap(), ["a", "b"]);
}

#[tokio::test]
async fn group_middleware_only_wraps_group_routes() {
    async fn admin_only(req: Request, next: Next) -> Result<Response> {
        if req.headers.get("x-admin").is_none() {
            return Err(Error::forbidden("admin only"));
        }
        next.run(req).await
    }

    let addr = spawn(
        App::new()
            .middleware(tag)
            .get("/public", |_req: Request| async {
                Ok(Response::text("public"))
            })
            .group("/admin", |g| {
                // registered before the routes: still applies to all of them
                g.middleware(admin_only)
                    .get("/panel", |_req: Request| async {
                        Ok(Response::text("panel"))
                    })
            }),
    )
    .await;

    assert_eq!(send(addr, "GET", "/public").await.0, 200);
    let (status, headers, _) = send(addr, "GET", "/admin/panel").await;
    assert_eq!(status, 403);
    // global middleware still wraps the group's rejection
    assert_eq!(headers["x-tagged"], "yes");
    let (status, _, body) = send_with(addr, "GET", "/admin/panel", &[("x-admin", "1")]).await;
    assert_eq!((status, body.as_str()), (200, "panel"));
}

#[derive(Clone, Debug, PartialEq)]
struct CurrentUser(String);

#[tokio::test]
async fn middleware_passes_data_via_extensions_and_peer_addr_is_set() {
    let addr = spawn(
        App::new()
            .middleware(|mut req: Request, next: Next| async move {
                req.extensions.insert(CurrentUser("ann".into()));
                next.run(req).await
            })
            .get("/me", |req: Request| async move {
                let user = req.extensions.get::<CurrentUser>().cloned();
                let peer = req.remote_addr.map(|a| a.ip().to_string());
                Ok(Response::json(serde_json::json!({
                    "user": user.map(|u| u.0),
                    "peer": peer,
                })))
            }),
    )
    .await;

    let (status, _, body) = send(addr, "GET", "/me").await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"peer":"127.0.0.1","user":"ann"}"#);
}

#[tokio::test]
async fn rate_limiter_answers_429_with_headers_and_skips_preflight() {
    use std::time::Duration;
    use ullun::rate_limit::RateLimiter;

    let addr = spawn(
        App::new()
            .middleware(RateLimiter::new(2, Duration::from_secs(60)))
            .get("/", |_req: Request| async { Ok(Response::text("ok")) }),
    )
    .await;

    let (s1, h1, _) = send(addr, "GET", "/").await;
    assert_eq!(
        (s1, h1["x-ratelimit-remaining"].to_str().unwrap()),
        (200, "1")
    );
    assert_eq!(send(addr, "OPTIONS", "/").await.0, 204); // automatic OPTIONS, not counted
    assert_eq!(send(addr, "GET", "/").await.0, 200);

    let (s3, h3, body) = send(addr, "GET", "/").await;
    assert_eq!(s3, 429);
    assert_eq!(h3["x-ratelimit-remaining"], "0");
    assert!(h3["retry-after"].to_str().unwrap().parse::<u64>().unwrap() >= 1);
    assert!(body.contains("Too many requests"));
}

#[tokio::test]
async fn rate_limiter_key_by_header_and_exemption() {
    use std::time::Duration;
    use ullun::rate_limit::RateLimiter;

    let limiter = RateLimiter::new(1, Duration::from_secs(60)).key_by(|req| {
        req.headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    });
    let addr = spawn(
        App::new()
            .middleware(limiter)
            .get("/", |_req: Request| async { Ok(Response::text("ok")) }),
    )
    .await;

    let key = |k: &'static str| [("x-api-key", k)];
    assert_eq!(send_with(addr, "GET", "/", &key("a")).await.0, 200);
    assert_eq!(send_with(addr, "GET", "/", &key("a")).await.0, 429);
    assert_eq!(send_with(addr, "GET", "/", &key("b")).await.0, 200);
    // no key -> exempt
    for _ in 0..3 {
        assert_eq!(send(addr, "GET", "/").await.0, 200);
    }
}

#[cfg(feature = "jwt")]
mod jwt_middleware {
    use super::*;
    use ullun::jwt::{self, Claims, JwtAuth};

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn bearer_auth_guards_a_group_and_exposes_claims() {
        let addr = spawn(
            App::new()
                .get("/open", |_req: Request| async {
                    Ok(Response::text("open"))
                })
                .group("/api", |g| {
                    g.middleware(JwtAuth::new(SECRET))
                        .get("/me", |req: Request| async move {
                            let claims = req.extensions.get::<Claims>().expect("claims");
                            Ok(Response::json(&claims.0))
                        })
                }),
        )
        .await;

        let good = jwt::encode(
            &serde_json::json!({ "sub": "ann", "exp": jwt::expires_in(60) }),
            SECRET.as_bytes(),
        )
        .unwrap();
        let expired = jwt::encode(
            &serde_json::json!({ "exp": jwt::now() - 5 }),
            SECRET.as_bytes(),
        )
        .unwrap();
        let forged = jwt::encode(
            &serde_json::json!({ "sub": "root" }),
            b"ffffffffffffffffffffffffffffffff",
        )
        .unwrap();

        assert_eq!(send(addr, "GET", "/open").await.0, 200);
        assert_eq!(send(addr, "GET", "/api/me").await.0, 401);

        let auth = |t: &str| format!("Bearer {t}");
        let (status, _, body) =
            send_with(addr, "GET", "/api/me", &[("authorization", &auth(&good))]).await;
        assert_eq!(status, 200);
        assert!(body.contains(r#""sub":"ann""#));

        for token in [&expired, &forged] {
            let (status, _, _) =
                send_with(addr, "GET", "/api/me", &[("authorization", &auth(token))]).await;
            assert_eq!(status, 401);
        }
        // wrong scheme
        let basic = format!("Basic {good}");
        assert_eq!(
            send_with(addr, "GET", "/api/me", &[("authorization", &basic)])
                .await
                .0,
            401
        );
    }
}

#[cfg(feature = "compression")]
mod compression_middleware {
    use super::*;
    use std::io::Read;
    use ullun::compression::Compression;

    fn app() -> App {
        let big = "ullun ".repeat(1000);
        let big2 = big.clone();
        App::new()
            .middleware(Compression::new())
            .get("/big", move |_req: Request| {
                let big = big.clone();
                async move { Ok(Response::text(big)) }
            })
            .get("/json", move |_req: Request| {
                let big = big2.clone();
                async move { Ok(Response::json(serde_json::json!({ "t": big }))) }
            })
            .get("/small", |_req: Request| async {
                Ok(Response::text("tiny"))
            })
            .get("/png", |_req: Request| async {
                Ok(Response::ok()
                    .with_header("content-type", "image/png")
                    .with_body("p".repeat(4000)))
            })
            .get("/missing-type", |_req: Request| async {
                Ok(Response::ok().with_body("x".repeat(4000)))
            })
    }

    async fn raw(
        addr: std::net::SocketAddr,
        path: &str,
        accept: Option<&str>,
    ) -> (u16, hyper::HeaderMap, Vec<u8>) {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut b = hyper::Request::builder()
            .uri(path)
            .header("host", "localhost");
        if let Some(a) = accept {
            b = b.header("accept-encoding", a);
        }
        let res = sender
            .send_request(b.body(Full::<Bytes>::default()).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        (
            status,
            headers,
            res.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
    }

    #[tokio::test]
    async fn gzips_large_text_and_json_responses() {
        let addr = spawn(app()).await;
        for (path, marker) in [("/big", "ullun ullun"), ("/json", "ullun ullun")] {
            let (status, headers, body) = raw(addr, path, Some("gzip")).await;
            assert_eq!(status, 200);
            assert_eq!(headers["content-encoding"], "gzip");
            assert_eq!(headers["vary"], "Accept-Encoding");
            assert!(body.len() < 1000, "{path}: {} bytes", body.len());
            let mut text = String::new();
            flate2::read::GzDecoder::new(&body[..])
                .read_to_string(&mut text)
                .unwrap();
            assert!(text.contains(marker), "{path}");
        }
    }

    #[tokio::test]
    async fn leaves_other_responses_untouched() {
        let addr = spawn(app()).await;

        let (_, h, body) = raw(addr, "/big", None).await;
        assert!(!h.contains_key("content-encoding") && body.len() == 6000);
        assert_eq!(h["vary"], "Accept-Encoding");

        let (_, h, _) = raw(addr, "/big", Some("gzip;q=0")).await;
        assert!(!h.contains_key("content-encoding"));

        let (_, h, body) = raw(addr, "/small", Some("gzip")).await;
        assert!(!h.contains_key("content-encoding") && body == b"tiny");

        for path in ["/png", "/missing-type"] {
            let (_, h, body) = raw(addr, path, Some("gzip")).await;
            assert!(
                !h.contains_key("content-encoding") && body.len() == 4000,
                "{path}"
            );
            assert!(!h.contains_key("vary"), "{path}");
        }
    }
}

#[tokio::test]
async fn one_middleware_instance_can_serve_several_groups() {
    use std::sync::Arc;
    use std::time::Duration;
    use ullun::rate_limit::RateLimiter;

    // a single budget of 2 requests across both groups
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let (a, b) = (limiter.clone(), limiter);
    let addr = spawn(
        App::new()
            .group("/a", move |g| {
                g.middleware(a)
                    .get("/x", |_req: Request| async { Ok(Response::text("a")) })
            })
            .group("/b", move |g| {
                g.middleware(b)
                    .get("/x", |_req: Request| async { Ok(Response::text("b")) })
            }),
    )
    .await;

    assert_eq!(send(addr, "GET", "/a/x").await.0, 200);
    assert_eq!(send(addr, "GET", "/b/x").await.0, 200);
    assert_eq!(send(addr, "GET", "/a/x").await.0, 429);
    assert_eq!(send(addr, "GET", "/b/x").await.0, 429);
}

#[tokio::test]
async fn validation_errors_render_as_422_with_fields() {
    use ullun::validate::Validator;

    let addr = spawn(App::new().post("/tasks", |req: Request| async move {
        let body: serde_json::Value = req.json()?;
        let mut v = Validator::new();
        v.len_between("title", body["title"].as_str().unwrap_or(""), 1, 5);
        v.range("priority", body["priority"].as_i64().unwrap_or(-1), 0, 3);
        v.error("title", "second problem on the same field");
        v.finish()?;
        Ok(Response::text("ok"))
    }))
    .await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .method("POST")
        .uri("/tasks")
        .header("host", "localhost")
        .body(Full::<Bytes>::from(r#"{"title":"","priority":9}"#))
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.status(), 422);
    let body: serde_json::Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"], "Validation failed");
    assert_eq!(body["status"], 422);
    assert_eq!(body["fields"]["title"].as_array().unwrap().len(), 2);
    assert_eq!(body["fields"]["priority"][0], "must be between 0 and 3");
}

#[tokio::test]
async fn sliding_rate_limiter_answers_429_with_headers() {
    use std::time::Duration;
    use ullun::rate_limit::RateLimiter;
    let addr = spawn(
        App::new()
            .middleware(RateLimiter::new(2, Duration::from_secs(60)).sliding_window())
            .get("/", |_req: Request| async { Ok(Response::text("ok")) }),
    )
    .await;
    let (s1, h1, _) = send(addr, "GET", "/").await;
    assert_eq!(
        (s1, h1["x-ratelimit-remaining"].to_str().unwrap()),
        (200, "1")
    );
    let (s2, _, _) = send(addr, "GET", "/").await;
    assert_eq!(s2, 200);
    let (s3, h3, _) = send(addr, "GET", "/").await;
    assert_eq!(s3, 429);
    let retry: u64 = h3["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry));
    assert_eq!(h3["x-ratelimit-remaining"], "0");
}
