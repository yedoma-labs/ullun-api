#![cfg(feature = "tls")]
//! HTTPS, certificate reload, mutual TLS and the HTTPS policy middleware

mod common;

use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use ullun::prelude::*;
use ullun::secure::{Hsts, RequireHttps};
use ullun::tls::TlsConfig;

fn ring() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
}

struct Pem {
    cert: String,
    key: String,
}

fn server_cert() -> Pem {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    Pem {
        cert: ck.cert.pem(),
        key: ck.signing_key.serialize_pem(),
    }
}

struct Ca {
    cert_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn new_ca(name: &str) -> Ca {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    let cert = params.self_signed(&key).unwrap();
    Ca {
        cert_pem: cert.pem(),
        issuer: Issuer::new(params, key),
    }
}

fn client_cert(ca: &Ca, name: &str) -> Pem {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, name);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let cert = params.signed_by(&key, &ca.issuer).unwrap();
    Pem {
        cert: cert.pem(),
        key: key.serialize_pem(),
    }
}

fn der(pem: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .map(|c| c.unwrap())
        .collect()
}

fn connector(trust: &[&str], client: Option<&Pem>) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    for pem in trust {
        for c in der(pem) {
            roots.add(c).unwrap();
        }
    }
    let builder = ClientConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    let config = match client {
        Some(p) => builder
            .with_client_auth_cert(
                der(&p.cert),
                PrivateKeyDer::from_pem_slice(p.key.as_bytes()).unwrap(),
            )
            .unwrap(),
        None => builder.with_no_client_auth(),
    };
    TlsConnector::from(Arc::new(config))
}

async fn start(app: App) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

/// One HTTPS request; (status, body)
async fn https(
    addr: SocketAddr,
    connector: &TlsConnector,
    method: &str,
    path: &str,
    body: &str,
) -> std::io::Result<(u16, hyper::HeaderMap, String)> {
    let tcp = TcpStream::connect(addr).await?;
    let tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(std::io::Error::other)?;
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let res = sender
        .send_request(req)
        .await
        .map_err(std::io::Error::other)?;
    let (status, headers) = (res.status().as_u16(), res.headers().clone());
    let text = String::from_utf8(
        res.into_body()
            .collect()
            .await
            .map_err(std::io::Error::other)?
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    Ok((status, headers, text))
}

fn info_app() -> App {
    App::new()
        .get("/info", |req: Request| async move {
            Ok(Response::json(serde_json::json!({
                "secure": req.is_secure(),
                "client_cert": req.client_certificate().map(|c| c.len()),
            })))
        })
        .post("/echo", |req: Request| async move {
            Ok(Response::text(
                String::from_utf8_lossy(&req.body).into_owned(),
            ))
        })
}

#[tokio::test]
async fn serves_https_and_marks_requests_secure() {
    let pem = server_cert();
    let addr = start(
        info_app().tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap()),
    )
    .await;
    let c = connector(&[&pem.cert], None);

    let (status, _, body) = https(addr, &c, "GET", "/info", "").await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "secure": true, "client_cert": null })
    );

    // bodies of any size travel through the TLS stream
    let big = "x".repeat(300_000);
    let (_, _, echoed) = https(addr, &c, "POST", "/echo", &big).await.unwrap();
    assert_eq!(echoed.len(), big.len());
    assert_eq!(https(addr, &c, "GET", "/missing", "").await.unwrap().0, 404);
}

#[tokio::test]
async fn untrusted_certificates_and_plain_http_are_refused_without_hurting_the_server() {
    let pem = server_cert();
    let other = server_cert();
    let addr = start(
        info_app().tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap()),
    )
    .await;

    // a client that does not trust our certificate
    assert!(
        https(addr, &connector(&[&other.cert], None), "GET", "/info", "")
            .await
            .is_err()
    );

    // plain HTTP and garbage on the TLS port
    for payload in [
        &b"GET /info HTTP/1.1\r\nhost: x\r\n\r\n"[..],
        b"\x16\x03\x01garbage",
        b"",
    ] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s.write_all(payload).await;
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf)).await;
        assert!(
            !String::from_utf8_lossy(&buf).contains("200 OK"),
            "plain HTTP must not be answered on the TLS port"
        );
    }
    // still fine afterwards
    assert_eq!(
        https(addr, &connector(&[&pem.cert], None), "GET", "/info", "")
            .await
            .unwrap()
            .0,
        200
    );
}

#[tokio::test]
async fn a_stalled_handshake_is_dropped_after_the_timeout_and_does_not_block_others() {
    let pem = server_cert();
    let tls = TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes())
        .unwrap()
        .handshake_timeout(Duration::from_millis(200));
    let addr = start(info_app().tls(tls)).await;

    // connects and says nothing
    let mut idle = TcpStream::connect(addr).await.unwrap();
    // ... meanwhile normal clients are served
    assert_eq!(
        https(addr, &connector(&[&pem.cert], None), "GET", "/info", "")
            .await
            .unwrap()
            .0,
        200
    );

    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), idle.read(&mut buf))
        .await
        .expect("server never closed the stalled connection")
        .unwrap_or(0);
    assert_eq!(n, 0, "the server hangs up");
}

#[tokio::test]
async fn certificates_can_be_replaced_without_a_restart() {
    let dir = std::env::temp_dir().join(format!("ullun-tls-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (cp, kp) = (dir.join("cert.pem"), dir.join("key.pem"));
    let (old, new) = (server_cert(), server_cert());
    std::fs::write(&cp, &old.cert).unwrap();
    std::fs::write(&kp, &old.key).unwrap();

    let tls = TlsConfig::from_pem_files(&cp, &kp).unwrap();
    let addr = start(info_app().tls(tls.clone())).await;
    let (trusts_old, trusts_new) = (connector(&[&old.cert], None), connector(&[&new.cert], None));
    assert_eq!(
        https(addr, &trusts_old, "GET", "/info", "")
            .await
            .unwrap()
            .0,
        200
    );
    assert!(https(addr, &trusts_new, "GET", "/info", "").await.is_err());

    // an already open connection keeps working across the switch
    let tcp = TcpStream::connect(addr).await.unwrap();
    let stream = trusts_old
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let get = |p: &'static str| {
        hyper::Request::builder()
            .uri(p)
            .header("host", "localhost")
            .body(Empty::<Bytes>::new())
            .unwrap()
    };
    assert_eq!(
        sender.send_request(get("/info")).await.unwrap().status(),
        200
    );

    // renewal arrives: a broken file first (old certificate stays), then a good one
    tokio::time::sleep(Duration::from_millis(1100)).await; // distinct modification times
    std::fs::write(&cp, "half written").unwrap();
    assert!(tls.reload().is_err());
    assert_eq!(
        https(addr, &trusts_old, "GET", "/info", "")
            .await
            .unwrap()
            .0,
        200,
        "previous certificate still in service"
    );

    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(&cp, &new.cert).unwrap();
    std::fs::write(&kp, &new.key).unwrap();
    assert!(tls.reload().unwrap());
    assert_eq!(
        https(addr, &trusts_new, "GET", "/info", "")
            .await
            .unwrap()
            .0,
        200
    );
    assert!(https(addr, &trusts_old, "GET", "/info", "").await.is_err());
    assert_eq!(
        sender.send_request(get("/info")).await.unwrap().status(),
        200,
        "the old connection survived"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn the_server_reloads_renewed_certificates_by_itself() {
    let dir = std::env::temp_dir().join(format!("ullun-tls-auto-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (cp, kp) = (dir.join("cert.pem"), dir.join("key.pem"));
    let (old, new) = (server_cert(), server_cert());
    std::fs::write(&cp, &old.cert).unwrap();
    std::fs::write(&kp, &old.key).unwrap();

    let tls = TlsConfig::from_pem_files(&cp, &kp).unwrap();
    let addr = start(
        info_app()
            .tls(tls)
            .tls_reload_every(Duration::from_millis(100)),
    )
    .await;
    let trusts_new = connector(&[&new.cert], None);
    assert!(https(addr, &trusts_new, "GET", "/info", "").await.is_err());

    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(&cp, &new.cert).unwrap();
    std::fs::write(&kp, &new.key).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if https(addr, &trusts_new, "GET", "/info", "").await.is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the renewed certificate was never picked up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn client_certificates_can_be_required() {
    let pem = server_cert();
    let ca = new_ca("Test CA");
    let rogue_ca = new_ca("Rogue CA");
    let tls = TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes())
        .unwrap()
        .client_auth(ca.cert_pem.as_bytes(), true)
        .unwrap();
    let addr = start(info_app().tls(tls)).await;

    let good = client_cert(&ca, "service-a");
    let (status, _, body) = https(
        addr,
        &connector(&[&pem.cert], Some(&good)),
        "GET",
        "/info",
        "",
    )
    .await
    .unwrap();
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["client_cert"].as_u64().unwrap() > 100,
        "the handler sees the certificate: {v}"
    );

    // no certificate, or one from another CA: the handshake fails
    assert!(
        https(addr, &connector(&[&pem.cert], None), "GET", "/info", "")
            .await
            .is_err()
    );
    let rogue = client_cert(&rogue_ca, "service-a");
    assert!(https(
        addr,
        &connector(&[&pem.cert], Some(&rogue)),
        "GET",
        "/info",
        ""
    )
    .await
    .is_err());
}

#[tokio::test]
async fn optional_client_certificates_let_handlers_decide_per_route() {
    let pem = server_cert();
    let ca = new_ca("Test CA");
    let tls = TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes())
        .unwrap()
        .client_auth(ca.cert_pem.as_bytes(), false)
        .unwrap();
    let app = App::new().get("/admin", |req: Request| async move {
        match req.client_certificate() {
            Some(_) => Ok(Response::text("welcome, certified client")),
            None => Err(Error::forbidden("client certificate required")),
        }
    });
    let addr = start(app.tls(tls)).await;

    let anonymous = connector(&[&pem.cert], None);
    assert_eq!(
        https(addr, &anonymous, "GET", "/admin", "")
            .await
            .unwrap()
            .0,
        403
    );
    let good = client_cert(&ca, "ops");
    let (status, _, body) = https(
        addr,
        &connector(&[&pem.cert], Some(&good)),
        "GET",
        "/admin",
        "",
    )
    .await
    .unwrap();
    assert_eq!((status, body.as_str()), (200, "welcome, certified client"));
    // presenting an untrusted certificate is worse than presenting none
    let rogue = client_cert(&new_ca("Rogue"), "ops");
    assert!(https(
        addr,
        &connector(&[&pem.cert], Some(&rogue)),
        "GET",
        "/admin",
        ""
    )
    .await
    .is_err());
}

#[tokio::test]
async fn graceful_shutdown_works_over_tls() {
    let pem = server_cert();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        info_app()
            .tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap())
            .serve_with_shutdown(listener, async {
                let _ = stopped.await;
            }),
    );
    let c = connector(&[&pem.cert], None);
    assert_eq!(https(addr, &c, "GET", "/info", "").await.unwrap().0, 200);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
    assert!(https(addr, &c, "GET", "/info", "").await.is_err());
}

#[cfg(feature = "websocket")]
#[tokio::test]
async fn websockets_work_over_tls() {
    use futures_util::{SinkExt, StreamExt};
    use ullun::websocket::{Message, WebSocket};

    let pem = server_cert();
    let app = App::new().ws("/ws", |_req: Request, mut s: WebSocket| async move {
        while let Some(Ok(Message::Text(t))) = s.recv().await {
            let _ = s.send_text(t.to_uppercase()).await;
        }
    });
    let addr =
        start(app.tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap())).await;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let tls = connector(&[&pem.cert], None)
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let (mut ws, res) = tokio_tungstenite::client_async("wss://localhost/ws", tls)
        .await
        .unwrap();
    assert_eq!(res.status(), 101);
    ws.send(tokio_tungstenite::tungstenite::Message::text("hello"))
        .await
        .unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().to_text().unwrap(),
        "HELLO"
    );
}

// ---- HTTPS policy (works without TLS) ----

#[tokio::test]
async fn plain_http_is_redirected_to_https_with_a_validated_host() {
    let redirect = start(App::new().middleware(RequireHttps::new().https_port(8443))).await;
    let raw = |request: &'static str| async move {
        let mut s = TcpStream::connect(redirect).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.unwrap();
        buf
    };

    let res =
        raw("GET /a/b?c=d HTTP/1.1\r\nhost: Example.com:8080\r\nconnection: close\r\n\r\n").await;
    assert!(res.starts_with("HTTP/1.1 308"), "{res}");
    assert!(
        res.contains("location: https://example.com:8443/a/b?c=d\r\n"),
        "{res}"
    );
    // the method and body survive a 308 redirect
    let res = raw("POST /x HTTP/1.1\r\nhost: api.example.com\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
    assert!(
        res.starts_with("HTTP/1.1 308")
            && res.contains("location: https://api.example.com:8443/x\r\n"),
        "{res}"
    );
    // hostile hosts and paths never end up in the Location header
    for bad_host in [
        "evil.com/path",
        "user@evil.com",
        "ev il.com",
        "evil.com:99999",
        "a..b",
    ] {
        let res = raw(Box::leak(
            format!("GET / HTTP/1.1\r\nhost: {bad_host}\r\nconnection: close\r\n\r\n")
                .into_boxed_str(),
        ))
        .await;
        assert!(res.starts_with("HTTP/1.1 400"), "{bad_host}: {res}");
    }
    let res =
        raw("GET //evil.example/x HTTP/1.1\r\nhost: example.com\r\nconnection: close\r\n\r\n")
            .await;
    assert!(
        res.contains("location: https://example.com:8443/evil.example/x\r\n"),
        "{res}"
    );
    let res = raw("GET / HTTP/1.1\r\nconnection: close\r\n\r\n").await;
    assert!(res.starts_with("HTTP/1.1 400"), "missing Host: {res}");
}

#[tokio::test]
async fn allowed_hosts_and_the_default_port() {
    let addr = start(
        App::new()
            .middleware(RequireHttps::new().allowed_hosts(["example.com", "www.example.com"])),
    )
    .await;
    let raw = |host: &'static str| async move {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!("GET /p HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.unwrap();
        buf
    };
    assert!(
        raw("WWW.example.com")
            .await
            .contains("location: https://www.example.com/p\r\n"),
        "no port part for 443"
    );
    assert!(raw("attacker.example").await.starts_with("HTTP/1.1 400"));
    assert!(raw("example.com.attacker.example")
        .await
        .starts_with("HTTP/1.1 400"));
}

#[tokio::test]
async fn forwarded_proto_is_only_believed_when_asked_to() {
    let app = |layer: RequireHttps| {
        App::new()
            .middleware(layer)
            .get("/", |_req: Request| async { Ok(Response::text("app")) })
    };
    let trusting = common::spawn(app(RequireHttps::new().trust_forwarded_proto())).await;
    let strict = common::spawn(app(RequireHttps::new())).await;

    assert_eq!(
        trusting
            .get("/")
            .header("x-forwarded-proto", "https")
            .send()
            .await
            .text(),
        "app"
    );
    assert_eq!(
        trusting
            .get("/")
            .header("x-forwarded-proto", "http")
            .send()
            .await
            .status,
        308
    );
    assert_eq!(trusting.get("/").send().await.status, 308);
    // a client claiming to be secure gets nothing from a server that does not trust the header
    assert_eq!(
        strict
            .get("/")
            .header("x-forwarded-proto", "https")
            .send()
            .await
            .status,
        308
    );
}

#[tokio::test]
async fn hsts_is_only_sent_on_secure_requests() {
    let plain = common::spawn(
        App::new()
            .middleware(Hsts::new().include_subdomains())
            .get("/", |_req: Request| async { Ok(Response::text("x")) }),
    )
    .await;
    assert_eq!(
        plain
            .get("/")
            .send()
            .await
            .header("strict-transport-security"),
        ""
    );

    let behind_proxy = common::spawn(
        App::new()
            .middleware(Hsts::new().trust_forwarded_proto().max_age(600))
            .get("/", |_req: Request| async { Ok(Response::text("x")) }),
    )
    .await;
    assert_eq!(
        behind_proxy
            .get("/")
            .header("x-forwarded-proto", "https")
            .send()
            .await
            .header("strict-transport-security"),
        "max-age=600"
    );
    assert_eq!(
        behind_proxy
            .get("/")
            .send()
            .await
            .header("strict-transport-security"),
        ""
    );

    let pem = server_cert();
    let addr = start(
        App::new()
            .middleware(Hsts::new().include_subdomains())
            .get("/", |_req: Request| async { Ok(Response::text("x")) })
            .tls(TlsConfig::from_pem(pem.cert.as_bytes(), pem.key.as_bytes()).unwrap()),
    )
    .await;
    let (_, headers, _) = https(addr, &connector(&[&pem.cert], None), "GET", "/", "")
        .await
        .unwrap();
    assert_eq!(
        headers["strict-transport-security"],
        "max-age=31536000; includeSubDomains"
    );
    // errors carry it too
    let (status, headers, _) = https(addr, &connector(&[&pem.cert], None), "GET", "/nope", "")
        .await
        .unwrap();
    assert_eq!(status, 404);
    assert!(headers.contains_key("strict-transport-security"));
}
