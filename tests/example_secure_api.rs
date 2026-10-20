#![cfg(feature = "tls")]
//! The secure API example: mutual TLS, allow-listed admins, HTTPS redirect

mod common;

#[allow(dead_code)]
#[path = "../examples/secure_api/app.rs"]
mod secure_api;

use common::pki::*;
use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use ullun::tls::TlsConfig;

struct Fixture {
    addr: std::net::SocketAddr,
    server_pem: Pem,
    ca: Ca,
    admin: Pem,
    admin_fp: String,
    other: Pem, // valid certificate, not on the allow-list
}

fn leaf_der(p: &Pem) -> Vec<u8> {
    der(&p.cert).remove(0).as_ref().to_vec()
}

async fn fixture() -> Fixture {
    let server_pem = server_cert();
    let ca = new_ca("Corp CA");
    let admin = client_cert(&ca, "ops-admin");
    let other = client_cert(&ca, "intern");
    let admin_fp = secure_api::fingerprint(&leaf_der(&admin));

    let tls = TlsConfig::from_pem(server_pem.cert.as_bytes(), server_pem.key.as_bytes())
        .unwrap()
        .client_auth(ca.cert_pem.as_bytes(), false)
        .unwrap();
    let app = secure_api::build(secure_api::Config {
        admin_fingerprints: vec![admin_fp.to_ascii_uppercase()],
    })
    .tls(tls);
    Fixture {
        addr: start(app).await,
        server_pem,
        ca,
        admin,
        admin_fp,
        other,
    }
}

async fn call(f: &Fixture, client: Option<&Pem>, method: &str, path: &str) -> (u16, Value) {
    let c = connector(&[&f.server_pem.cert], client);
    let (status, _, body) = https(f.addr, &c, method, path, "").await.unwrap();
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn public_routes_work_without_a_certificate() {
    let f = fixture().await;
    let (status, body) = call(&f, None, "GET", "/api/whoami").await;
    assert_eq!(status, 200);
    assert_eq!(
        (body["secure"].as_bool(), body["admin"].as_bool()),
        (Some(true), Some(false))
    );
    assert!(body["certificate"].is_null());
    assert_eq!(call(&f, None, "GET", "/healthz").await.1["status"], "ok");
    assert!(
        call(&f, None, "GET", "/api/time").await.1["unix"]
            .as_u64()
            .unwrap()
            > 1_700_000_000
    );
}

#[tokio::test]
async fn admin_routes_need_a_certificate_that_is_on_the_allow_list() {
    let f = fixture().await;

    // no certificate: 401, certified but not an admin: 403, admin: 200
    assert_eq!(call(&f, None, "GET", "/admin/status").await.0, 401);
    let (status, body) = call(&f, Some(&f.other), "GET", "/admin/status").await;
    assert_eq!(status, 403);
    assert!(body["error"].as_str().unwrap().contains("not allowed"));
    let (status, body) = call(&f, Some(&f.admin), "GET", "/admin/status").await;
    assert_eq!(status, 200);
    assert_eq!(body["you"], f.admin_fp, "the handler knows who is calling");
    assert_eq!(
        call(&f, Some(&f.admin), "POST", "/admin/maintenance")
            .await
            .0,
        200
    );
    assert_eq!(
        call(&f, Some(&f.other), "POST", "/admin/maintenance")
            .await
            .0,
        403
    );
    assert_eq!(call(&f, None, "POST", "/admin/maintenance").await.0, 401);

    // the allow-list is matched case-insensitively (it was configured in upper case)
    let who = call(&f, Some(&f.admin), "GET", "/api/whoami").await.1;
    assert_eq!(
        (who["admin"].as_bool(), who["certificate"].as_str()),
        (Some(true), Some(f.admin_fp.as_str()))
    );
    let who = call(&f, Some(&f.other), "GET", "/api/whoami").await.1;
    assert_eq!(who["admin"], false);
    assert_ne!(who["certificate"].as_str().unwrap(), f.admin_fp);
}

#[tokio::test]
async fn certificates_from_other_authorities_never_reach_the_application() {
    let f = fixture().await;
    let rogue_ca = new_ca("Rogue CA");
    // same subject name as the real admin, signed by someone else
    let impostor = client_cert(&rogue_ca, "ops-admin");
    let c = connector(&[&f.server_pem.cert], Some(&impostor));
    assert!(https(f.addr, &c, "GET", "/admin/status", "").await.is_err());

    // a self-signed certificate claiming to be the admin's, too
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params
        .distinguished_name
        .push(DnType::CommonName, "ops-admin");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let forged = Pem {
        cert: params.self_signed(&key).unwrap().pem(),
        key: key.serialize_pem(),
    };
    assert!(https(
        f.addr,
        &connector(&[&f.server_pem.cert], Some(&forged)),
        "GET",
        "/admin/status",
        ""
    )
    .await
    .is_err());
    let _ = &f.ca;
}

#[tokio::test]
async fn responses_carry_hsts_and_a_request_id() {
    let f = fixture().await;
    let c = connector(&[&f.server_pem.cert], None);
    let (_, headers, _) = https(f.addr, &c, "GET", "/api/time", "").await.unwrap();
    assert_eq!(
        headers["strict-transport-security"],
        "max-age=31536000; includeSubDomains"
    );
    assert_eq!(headers["x-request-id"].to_str().unwrap().len(), 16);
}

#[tokio::test]
async fn the_http_listener_only_redirects() {
    let addr = start(secure_api::redirect_app(8443)).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        b"GET /admin/status?x=1 HTTP/1.1\r\nhost: api.example.com\r\nconnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 308"), "{out}");
    assert!(
        out.contains("location: https://api.example.com:8443/admin/status?x=1\r\n"),
        "{out}"
    );
}

#[test]
fn fingerprints_are_sha256_hex() {
    assert_eq!(
        secure_api::fingerprint(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        secure_api::fingerprint(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
