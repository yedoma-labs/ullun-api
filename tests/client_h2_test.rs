//! The outbound client over HTTP/2 (ALPN on https, prior knowledge on http)

mod common;

use common::spawn;
use std::time::Duration;
use ullun::client::HttpClient;
use ullun::prelude::*;

fn app() -> App {
    App::new()
        .get("/hello", |req: Request| async move {
            Ok(Response::json(serde_json::json!({
                "ua": req.headers.get("user-agent").and_then(|v| v.to_str().ok()),
                "x": req.headers.get("x-default").and_then(|v| v.to_str().ok()),
                "q": req.query.get("q"),
                "peer_port": req.remote_addr.map(|a| a.port()),
            })))
        })
        .post("/echo", |req: Request| async move {
            Ok(Response::text(
                String::from_utf8_lossy(&req.body).into_owned(),
            ))
        })
        .get("/slow", |req: Request| async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(Response::text(format!(
                "{}",
                req.remote_addr.map(|a| a.port()).unwrap_or(0)
            )))
        })
}

#[tokio::test]
async fn plain_http_stays_http1_unless_told_otherwise() {
    let s = spawn(app().h2c(true)).await;
    let res = HttpClient::new()
        .get(&format!("http://{}/hello", s.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(res.version(), http::Version::HTTP_11);
}

#[tokio::test]
async fn prior_knowledge_speaks_http2_over_one_connection() {
    let s = spawn(app().h2c(true)).await;
    let client = HttpClient::new()
        .http2_prior_knowledge()
        .default_header("x-default", "yes");
    let base = format!("http://{}", s.addr);

    let res = client
        .get(&format!("{base}/hello?q=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.version(), http::Version::HTTP_2);
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().unwrap();
    assert_eq!(body["q"], "1");
    assert_eq!(body["x"], "yes");
    assert!(body["ua"].as_str().unwrap().starts_with("ullun-api/"));
    let first_port = body["peer_port"].clone();

    // the same connection serves the next request
    let again: serde_json::Value = client
        .get(&format!("{base}/hello"))
        .send()
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(again["peer_port"], first_port);

    let echoed = client
        .post(&format!("{base}/echo"))
        .body("ping")
        .send()
        .await
        .unwrap();
    assert_eq!(echoed.text().unwrap(), "ping");
}

#[tokio::test]
async fn requests_are_multiplexed_on_a_single_connection() {
    // a second connection would be refused
    let s = spawn(app().h2c(true).max_connections(1)).await;
    let client = HttpClient::new().http2_prior_knowledge();
    let url = format!("http://{}/slow", s.addr);

    // connect once so the connection is shared, then fire in parallel
    client.get(&url).send().await.unwrap();
    let started = std::time::Instant::now();
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let (client, url) = (client.clone(), url.clone());
            tokio::spawn(async move { client.get(&url).send().await.unwrap().text().unwrap() })
        })
        .collect();
    let mut ports = std::collections::HashSet::new();
    for call in calls {
        ports.insert(call.await.unwrap());
    }
    assert_eq!(ports.len(), 1, "one connection, one peer port: {ports:?}");
    assert!(started.elapsed() < Duration::from_millis(700));
}

#[tokio::test]
async fn a_dead_connection_is_replaced() {
    let first = spawn(app().h2c(true)).await;
    let client = HttpClient::new().http2_prior_knowledge();
    client
        .get(&format!("http://{}/hello", first.addr))
        .send()
        .await
        .unwrap();

    // unreachable host: error, not a hang, and the client keeps working
    let dead = client
        .get("http://127.0.0.1:1/hello")
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    assert!(dead.is_err());
    assert_eq!(
        client
            .get(&format!("http://{}/hello", first.addr))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn h2c_against_an_http1_only_server_is_an_error() {
    let s = spawn(app()).await;
    let r = HttpClient::new()
        .http2_prior_knowledge()
        .get(&format!("http://{}/hello", s.addr))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    assert!(r.is_err(), "{r:?}");
}

#[cfg(all(feature = "client-tls", feature = "tls"))]
mod https {
    use super::*;
    use common::pki::server_cert;
    use ullun::tls::TlsConfig;

    async fn start_tls() -> (std::net::SocketAddr, String) {
        let pem = server_cert();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = app().tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap());
        tokio::spawn(app.serve(listener));
        (addr, pem.cert)
    }

    #[tokio::test]
    async fn https_negotiates_http2_and_reuses_the_connection() {
        let (addr, cert) = start_tls().await;
        let client = HttpClient::new()
            .add_root_certificates_pem(cert.as_bytes())
            .unwrap();
        let url = format!("https://localhost:{}/hello", addr.port());

        let a = client.get(&url).send().await.unwrap();
        assert_eq!(a.version(), http::Version::HTTP_2);
        let b = client.get(&url).send().await.unwrap();
        assert_eq!(b.version(), http::Version::HTTP_2);
        let (a, b): (serde_json::Value, serde_json::Value) = (a.json().unwrap(), b.json().unwrap());
        assert_eq!(a["peer_port"], b["peer_port"]);
    }

    #[tokio::test]
    async fn http1_only_opts_out_of_http2() {
        let (addr, cert) = start_tls().await;
        let client = HttpClient::new()
            .add_root_certificates_pem(cert.as_bytes())
            .unwrap()
            .http1_only();
        let res = client
            .get(&format!("https://localhost:{}/hello", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(res.version(), http::Version::HTTP_11);
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn parallel_https_requests_share_one_connection() {
        let (addr, cert) = start_tls().await;
        let client = HttpClient::new()
            .add_root_certificates_pem(cert.as_bytes())
            .unwrap();
        let url = format!("https://localhost:{}/slow", addr.port());
        client.get(&url).send().await.unwrap();
        let calls: Vec<_> = (0..5)
            .map(|_| {
                let (client, url) = (client.clone(), url.clone());
                tokio::spawn(async move { client.get(&url).send().await.unwrap().text().unwrap() })
            })
            .collect();
        let mut ports = std::collections::HashSet::new();
        for call in calls {
            ports.insert(call.await.unwrap());
        }
        assert_eq!(ports.len(), 1, "{ports:?}");
    }
}
