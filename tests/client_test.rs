//! The outbound HTTP client against real servers

mod common;

use common::spawn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ullun::client::{ClientError, HttpClient};
use ullun::prelude::*;

struct Hits(Arc<AtomicUsize>);

fn url(s: &common::Server, path: &str) -> String {
    format!("http://{}{}", s.addr, path)
}

fn upstream() -> (App, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app =
        App::new()
            .state(Hits(hits.clone()))
            .get("/hello", |req: Request| async move {
                Ok(Response::json(serde_json::json!({
                    "ua": req.headers.get("user-agent").and_then(|v| v.to_str().ok()),
                    "x": req.headers.get("x-default").and_then(|v| v.to_str().ok()),
                    "q": req.query.get("q"),
                    "peer_port": req.remote_addr.map(|a| a.port()),
                })))
            })
            .post("/echo", |req: Request| async move {
                let ct = req
                    .headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                Ok(Response::text(format!(
                    "{ct}|{}",
                    String::from_utf8_lossy(&req.body)
                )))
            })
            .get("/slow", |_req: Request| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(Response::text("late"))
            })
            .get("/flaky", |req: Request| async move {
                let n = req.state::<Hits>()?.0.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Ok(Response::text("busy")
                        .with_status(http::StatusCode::SERVICE_UNAVAILABLE)
                        .with_header("retry-after", "0"))
                } else {
                    Ok(Response::text("ok"))
                }
            })
            .post("/flaky", |req: Request| async move {
                let n = req.state::<Hits>()?.0.fetch_add(1, Ordering::SeqCst);
                if n < 1 {
                    Ok(Response::text("busy").with_status(http::StatusCode::SERVICE_UNAVAILABLE))
                } else {
                    Ok(Response::text("ok"))
                }
            })
            .get("/big", |_req: Request| async {
                Ok(Response::text("x".repeat(10_000)))
            })
            .get("/to-hello", |_req: Request| async {
                Ok(Response::redirect("/hello?q=redirected"))
            })
            .post("/see-other", |_req: Request| async {
                Ok(Response::new(http::StatusCode::SEE_OTHER).with_header("location", "/hello"))
            })
            .post("/temporary", |_req: Request| async {
                Ok(Response::new(http::StatusCode::TEMPORARY_REDIRECT)
                    .with_header("location", "/echo"))
            })
            .get("/loop", |_req: Request| async {
                Ok(Response::redirect("/loop"))
            })
            .get("/auth", |req: Request| async move {
                Ok(Response::text(
                    req.headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("none")
                        .to_string(),
                ))
            });
    (app, hits)
}

#[tokio::test]
async fn get_with_headers_query_and_defaults() {
    let (app, _) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new()
        .user_agent("probe/1")
        .default_header("x-default", "yes");
    let res = client
        .get(&url(&s, "/hello"))
        .query(&[("q", "a b&c")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.is_success());
    let body: serde_json::Value = res.json().unwrap();
    assert_eq!(body["ua"], "probe/1");
    assert_eq!(body["x"], "yes");
    assert_eq!(body["q"], "a b&c");
}

#[tokio::test]
async fn json_form_and_raw_bodies() {
    let (app, _) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new();
    let json = client
        .post(&url(&s, "/echo"))
        .json(&serde_json::json!({ "a": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(json.text().unwrap(), "application/json|{\"a\":1}");
    let form = client
        .post(&url(&s, "/echo"))
        .form(&[("name", "x y")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        form.text().unwrap(),
        "application/x-www-form-urlencoded|name=x+y"
    );
    let raw = client
        .post(&url(&s, "/echo"))
        .header("content-type", "text/plain")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.text().unwrap(), "text/plain|hello");
}

#[tokio::test]
async fn connections_are_pooled() {
    let (app, _) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new();
    let port = |r: ullun::client::ClientResponse| {
        r.json::<serde_json::Value>().unwrap()["peer_port"].clone()
    };
    let a = port(client.get(&url(&s, "/hello")).send().await.unwrap());
    let b = port(client.get(&url(&s, "/hello")).send().await.unwrap());
    assert_eq!(a, b, "second call should reuse the connection");

    let no_pool = HttpClient::new().pool_idle_timeout(Duration::ZERO);
    let c = port(no_pool.get(&url(&s, "/hello")).send().await.unwrap());
    let d = port(no_pool.get(&url(&s, "/hello")).send().await.unwrap());
    assert_ne!(c, d);
}

#[tokio::test]
async fn timeouts_and_connection_errors() {
    let (app, _) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new().timeout(Duration::from_millis(200));
    assert!(matches!(
        client.get(&url(&s, "/slow")).send().await,
        Err(ClientError::Timeout)
    ));
    // per request override
    let ok = HttpClient::new()
        .timeout(Duration::from_millis(50))
        .get(&url(&s, "/hello"))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    assert!(ok.is_ok());

    // nothing listens on this port any more
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    assert!(matches!(
        HttpClient::new()
            .get(&format!("http://{dead}/"))
            .send()
            .await,
        Err(ClientError::Connect(_))
    ));
    assert!(matches!(
        HttpClient::new().get("ftp://example.com/").send().await,
        Err(ClientError::InvalidUrl(_))
    ));
    assert!(matches!(
        HttpClient::new().get("/relative").send().await,
        Err(ClientError::InvalidUrl(_))
    ));
}

#[tokio::test]
async fn retries_follow_retry_after_and_respect_idempotency() {
    let (app, hits) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new()
        .retries(3)
        .retry_backoff(Duration::from_millis(5));

    let res = client.get(&url(&s, "/flaky")).send().await.unwrap();
    assert_eq!((res.status(), res.text().unwrap().as_str()), (200, "ok"));
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    // a POST without an idempotency key is not repeated
    hits.store(0, Ordering::SeqCst);
    let res = client.post(&url(&s, "/flaky")).send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // with a key it is
    hits.store(0, Ordering::SeqCst);
    let res = client
        .post(&url(&s, "/flaky"))
        .header("idempotency-key", "k1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // retries are off by default
    hits.store(0, Ordering::SeqCst);
    let res = HttpClient::new()
        .get(&url(&s, "/flaky"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
}

#[tokio::test]
async fn retries_connection_failures_then_gives_up() {
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let started = std::time::Instant::now();
    let r = HttpClient::new()
        .retries(2)
        .retry_backoff(Duration::from_millis(30))
        .get(&format!("http://{dead}/"))
        .send()
        .await;
    assert!(matches!(r, Err(ClientError::Connect(_))));
    // two backoffs of at least 30 ms and 60 ms happened
    assert!(started.elapsed() >= Duration::from_millis(80));
}

#[tokio::test]
async fn response_size_is_capped() {
    let (app, _) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new().max_response_bytes(1000);
    assert!(matches!(
        client.get(&url(&s, "/big")).send().await,
        Err(ClientError::TooLarge(1000))
    ));
    let ok = HttpClient::new()
        .get(&url(&s, "/big"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.bytes().len(), 10_000);
}

#[tokio::test]
async fn redirects_are_followed_with_the_right_method() {
    let (app, _) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new();

    let res = client.get(&url(&s, "/to-hello")).send().await.unwrap();
    assert_eq!(res.json::<serde_json::Value>().unwrap()["q"], "redirected");

    // 303 turns a POST into a GET
    let res = client
        .post(&url(&s, "/see-other"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.json::<serde_json::Value>().unwrap().get("ua").is_some());

    // 307 keeps method and body
    let res = client
        .post(&url(&s, "/temporary"))
        .header("content-type", "text/plain")
        .body("kept")
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().unwrap(), "text/plain|kept");

    assert!(matches!(
        client.get(&url(&s, "/loop")).send().await,
        Err(ClientError::TooManyRedirects)
    ));
    let raw = HttpClient::new()
        .max_redirects(0)
        .get(&url(&s, "/to-hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), 303);
    assert!(raw.header("location").starts_with("/hello"));
}

#[tokio::test]
async fn credentials_are_not_sent_to_another_origin_after_a_redirect() {
    let (app_b, _) = upstream();
    let b = spawn(app_b).await;
    let target = url(&b, "/auth");
    let app_a = App::new().get("/go", move |_req: Request| {
        let target = target.clone();
        async move { Ok(Response::redirect(target)) }
    });
    let a = spawn(app_a).await;

    let res = HttpClient::new()
        .get(&url(&a, "/go"))
        .bearer("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().unwrap(), "none");

    // same origin keeps them
    let (app_c, _) = upstream();
    let c = spawn(app_c.get("/same", |_req: Request| async {
        Ok(Response::redirect("/auth"))
    }))
    .await;
    let res = HttpClient::new()
        .get(&url(&c, "/same"))
        .bearer("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().unwrap(), "Bearer secret");
}

#[tokio::test]
async fn public_only_refuses_loopback_and_private_targets() {
    let (app, _) = upstream();
    let s = spawn(app).await;
    let client = HttpClient::new().public_only();
    assert!(matches!(
        client.get(&url(&s, "/hello")).send().await,
        Err(ClientError::Blocked(_))
    ));
    for target in [
        "http://169.254.169.254/latest/meta-data",
        "http://10.0.0.1/",
        "http://[::1]/",
    ] {
        assert!(
            matches!(
                client.get(target).send().await,
                Err(ClientError::Blocked(_))
            ),
            "{target}"
        );
    }
}

#[tokio::test]
async fn client_errors_become_gateway_errors_in_handlers() {
    let e: Error = ClientError::Timeout.into();
    assert_eq!(e.status_code(), 504);
    let e: Error = ClientError::Connect("refused".into()).into();
    assert_eq!(e.status_code(), 502);

    let (app, _) = upstream();
    let up = spawn(app).await;
    let target = url(&up, "/slow");
    let proxy = App::new().get("/proxy", move |_req: Request| {
        let target = target.clone();
        async move {
            let r = HttpClient::new()
                .timeout(Duration::from_millis(100))
                .get(&target)
                .send()
                .await?;
            Ok(Response::text(r.text()?))
        }
    });
    let s = spawn(proxy).await;
    assert_eq!(s.get("/proxy").send().await.status, 504);
}

#[test]
fn error_messages_are_readable() {
    assert_eq!(ClientError::Timeout.to_string(), "request timed out");
    assert!(ClientError::Blocked("10.0.0.1".parse().unwrap())
        .to_string()
        .contains("10.0.0.1"));
    assert!(ClientError::TooLarge(5).to_string().contains('5'));
}

#[cfg(feature = "client-tls")]
mod https {
    use super::*;
    use common::pki::server_cert;
    use ullun::tls::TlsConfig;

    async fn start_tls() -> (std::net::SocketAddr, String) {
        let pem = server_cert();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = App::new()
            .tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap())
            .get("/secure", |req: Request| async move {
                Ok(Response::text(format!("secure={}", req.is_secure())))
            });
        tokio::spawn(app.serve(listener));
        (addr, pem.cert)
    }

    #[tokio::test]
    async fn https_with_a_trusted_private_ca() {
        let (addr, cert) = start_tls().await;
        let client = HttpClient::new()
            .add_root_certificates_pem(cert.as_bytes())
            .unwrap();
        let url = format!("https://localhost:{}/secure", addr.port());
        let res = client.get(&url).send().await.unwrap();
        assert_eq!(res.text().unwrap(), "secure=true");
        // second call reuses the TLS connection
        assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn untrusted_certificates_are_rejected() {
        let (addr, _) = start_tls().await;
        let r = HttpClient::new()
            .get(&format!("https://localhost:{}/secure", addr.port()))
            .send()
            .await;
        assert!(
            matches!(r, Err(ClientError::Tls(_)) | Err(ClientError::Protocol(_))),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn bad_root_certificate_data_is_an_error() {
        assert!(HttpClient::new()
            .add_root_certificates_pem(b"not pem")
            .is_err());
    }
}

#[cfg(not(feature = "client-tls"))]
#[tokio::test]
async fn https_needs_the_feature() {
    let r = HttpClient::new().get("https://localhost:1/").send().await;
    assert!(matches!(r, Err(ClientError::Tls(_))), "{r:?}");
}
