#![cfg(feature = "tls")]
//! HTTP/2 over TLS (ALPN)

mod common;

use common::pki::*;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use ullun::prelude::*;
use ullun::tls::TlsConfig;

fn h2_connector(cert_pem: &str, alpn: &[&[u8]]) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    for c in der(cert_pem) {
        roots.add(c).unwrap();
    }
    let mut config = ClientConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    TlsConnector::from(Arc::new(config))
}

async fn server() -> (std::net::SocketAddr, String) {
    let pem = server_cert();
    let app = App::new()
        .get("/info", |req: Request| async move {
            Ok(Response::json(serde_json::json!({
                "secure": req.is_secure(),
                "host": req.headers.get("host").and_then(|v| v.to_str().ok()),
                "path": req.uri.path(),
            })))
        })
        .post("/echo", |req: Request| async move {
            Ok(Response::text(
                String::from_utf8_lossy(&req.body).into_owned(),
            ))
        })
        .get("/slow", |_req: Request| async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(Response::text("slow"))
        })
        .get("/stream", |_req: Request| async {
            let (tx, res) = Response::stream();
            tokio::spawn(async move {
                for n in 1..=3 {
                    let _ = tx.send(format!("{n};")).await;
                }
            });
            Ok(res)
        })
        .tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap());
    (start(app).await, pem.cert)
}

type H2Sender = hyper::client::conn::http2::SendRequest<Full<Bytes>>;

async fn h2_client(addr: std::net::SocketAddr, cert: &str) -> (H2Sender, Option<Vec<u8>>) {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let tls = h2_connector(cert, &[b"h2", b"http/1.1"])
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let negotiated = tls.get_ref().1.alpn_protocol().map(|p| p.to_vec());
    let (sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    (sender, negotiated)
}

fn request(method: &str, uri: &str, body: &str) -> hyper::Request<Full<Bytes>> {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

#[tokio::test]
async fn clients_that_offer_h2_get_http2() {
    let (addr, cert) = server().await;
    let (mut sender, negotiated) = h2_client(addr, &cert).await;
    assert_eq!(negotiated.as_deref(), Some(&b"h2"[..]));

    let res = sender
        .send_request(request(
            "GET",
            &format!("https://localhost:{}/info", addr.port()),
            "",
        ))
        .await
        .unwrap();
    assert_eq!(res.version(), http::Version::HTTP_2);
    assert_eq!(res.status(), 200);
    let body: serde_json::Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["secure"], true);
    assert_eq!(body["path"], "/info");
    // :authority is exposed as the Host header handlers expect
    assert_eq!(body["host"], format!("localhost:{}", addr.port()));
}

#[tokio::test]
async fn http1_clients_still_work_on_the_same_port() {
    let (addr, cert) = server().await;
    let c = h2_connector(&cert, &[b"http/1.1"]);
    let (status, _, body) = https(addr, &c, "GET", "/info", "").await.unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("\"secure\":true"));
    // a client without ALPN at all speaks HTTP/1.1 too
    let plain = connector(&[&cert], None);
    assert_eq!(
        https(addr, &plain, "GET", "/info", "").await.unwrap().0,
        200
    );
}

#[tokio::test]
async fn requests_are_multiplexed_on_one_connection() {
    let (addr, cert) = server().await;
    let (sender, _) = h2_client(addr, &cert).await;
    let started = std::time::Instant::now();
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let mut s = sender.clone();
            let uri = format!("https://localhost:{}/slow", addr.port());
            tokio::spawn(async move {
                let res = s.send_request(request("GET", &uri, "")).await.unwrap();
                res.into_body().collect().await.unwrap().to_bytes()
            })
        })
        .collect();
    for c in calls {
        assert_eq!(c.await.unwrap(), "slow");
    }
    // eight 200 ms requests in parallel, not one after another
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn bodies_and_streams_work_over_http2() {
    let (addr, cert) = server().await;
    let (mut sender, _) = h2_client(addr, &cert).await;
    let big = "y".repeat(200_000);
    let res = sender
        .send_request(request(
            "POST",
            &format!("https://localhost:{}/echo", addr.port()),
            &big,
        ))
        .await
        .unwrap();
    assert_eq!(
        res.into_body().collect().await.unwrap().to_bytes().len(),
        big.len()
    );

    let res = sender
        .send_request(request(
            "GET",
            &format!("https://localhost:{}/stream", addr.port()),
            "",
        ))
        .await
        .unwrap();
    assert_eq!(
        res.into_body().collect().await.unwrap().to_bytes(),
        "1;2;3;"
    );

    // a miss is a normal 404 over h2 as well
    let res = sender
        .send_request(request(
            "GET",
            &format!("https://localhost:{}/none", addr.port()),
            "",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let _ = Empty::<Bytes>::new();
}

#[tokio::test]
async fn shutdown_lets_running_http2_requests_finish() {
    let pem = server_cert();
    let app = App::new()
        .get("/slow", |_req: Request| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(Response::text("finished"))
        })
        .tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(app.serve_with_shutdown(listener, async {
        let _ = stopped.await;
    }));

    let (mut sender, _) = h2_client(addr, &pem.cert).await;
    let uri = format!("https://localhost:{}/slow", addr.port());
    let pending = tokio::spawn(async move {
        let res = sender.send_request(request("GET", &uri, "")).await.unwrap();
        res.into_body().collect().await.unwrap().to_bytes()
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.send(()).unwrap();

    assert_eq!(pending.await.unwrap(), "finished");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
}
