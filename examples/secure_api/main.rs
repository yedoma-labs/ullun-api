//! HTTPS service with mutual-TLS protected admin routes (feature `tls`)
//!
//! * HTTPS with certificates that are picked up again when they are renewed
//!   (no restart, open connections keep running)
//! * a plain-HTTP listener that only redirects to HTTPS
//! * HSTS on every HTTPS response
//! * `/api/*` is open; `/admin/*` needs a client certificate signed by your CA
//!   *and* whose fingerprint is on the allow-list
//!
//! Configuration (environment):
//!   HTTPS_PORT          default 8443
//!   HTTP_PORT           redirect listener, default 8080 (0 = off)
//!   TLS_CERT, TLS_KEY   PEM files of the server certificate chain and key
//!   CLIENT_CA           PEM file of the CA that signs admin client certificates
//!   ADMIN_FINGERPRINTS  comma separated SHA-256 fingerprints (hex) of admin certificates
//!   DEV                 1 = create throwaway certificates in ./dev-certs and print curl commands
//!
//! Run:   DEV=1 cargo run --example secure_api --features tls

mod app;

use std::time::Duration;
use ullun::tls::TlsConfig;

/// Throwaway PKI for trying the example: CA, server certificate for localhost,
/// and one admin client certificate
fn dev_certs() -> (String, String, String, String) {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    };
    let dir = std::path::Path::new("dev-certs");
    std::fs::create_dir_all(dir).expect("create dev-certs");

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "ullun dev CA");
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().unwrap();
    let server_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(Vec::new()).unwrap();
    client_params
        .distinguished_name
        .push(DnType::CommonName, "dev admin");
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();

    let write = |name: &str, content: String| {
        std::fs::write(dir.join(name), &content).expect("write dev cert");
        content
    };
    write("ca.pem", ca_cert.pem());
    let cert = write("server.pem", server_cert.pem());
    let key = write("server.key", server_key.serialize_pem());
    write("client.pem", client_cert.pem());
    write("client.key", client_key.serialize_pem());
    let fingerprint = app::fingerprint(client_cert.der());
    (
        cert,
        key,
        std::fs::read_to_string(dir.join("ca.pem")).unwrap(),
        fingerprint,
    )
}

#[tokio::main]
async fn main() {
    let env = |k: &str| std::env::var(k).ok();
    let dev = env("DEV").as_deref() == Some("1");
    let https_port = env("HTTPS_PORT").unwrap_or_else(|| "8443".into());
    let http_port: u16 = env("HTTP_PORT")
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let (tls, mut fingerprints) = if dev {
        let (cert, key, ca, fp) = dev_certs();
        println!("dev certificates written to ./dev-certs, try:");
        println!("  curl --cacert dev-certs/ca.pem https://localhost:{https_port}/api/whoami");
        println!("  curl --cacert dev-certs/ca.pem --cert dev-certs/client.pem --key dev-certs/client.key https://localhost:{https_port}/admin/status");
        (
            TlsConfig::from_pem(cert.as_bytes(), key.as_bytes())
                .and_then(|t| t.client_auth(ca.as_bytes(), false)),
            vec![fp],
        )
    } else {
        let (Some(cert), Some(key), Some(ca)) = (env("TLS_CERT"), env("TLS_KEY"), env("CLIENT_CA"))
        else {
            eprintln!("TLS_CERT, TLS_KEY and CLIENT_CA are required (or DEV=1)");
            std::process::exit(2);
        };
        let ca_pem = std::fs::read(&ca).unwrap_or_else(|e| {
            eprintln!("cannot read CLIENT_CA {ca}: {e}");
            std::process::exit(2);
        });
        (
            TlsConfig::from_pem_files(&cert, &key).and_then(|t| t.client_auth(&ca_pem, false)),
            Vec::new(),
        )
    };
    let tls = tls.unwrap_or_else(|e| {
        eprintln!("{}", e.message());
        std::process::exit(2);
    });
    fingerprints.extend(
        env("ADMIN_FINGERPRINTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty()),
    );

    let https = app::build(app::Config {
        admin_fingerprints: fingerprints,
    })
    .tls(tls)
    .tls_reload_every(Duration::from_secs(3600))
    .run(Box::leak(format!("0.0.0.0:{https_port}").into_boxed_str()));
    if http_port == 0 {
        https.await.unwrap();
    } else {
        let redirect = app::redirect_app(https_port.parse().unwrap_or(443))
            .run(Box::leak(format!("0.0.0.0:{http_port}").into_boxed_str()));
        let (a, b) = tokio::join!(https, redirect);
        a.unwrap();
        b.unwrap();
    }
}
