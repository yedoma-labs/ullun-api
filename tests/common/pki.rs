//! Throwaway certificates and a TLS test client (feature `tls`)

#![allow(dead_code)]

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use ullun::prelude::*;

pub fn ring() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
}

pub struct Pem {
    pub cert: String,
    pub key: String,
}

pub fn server_cert() -> Pem {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    Pem {
        cert: ck.cert.pem(),
        key: ck.signing_key.serialize_pem(),
    }
}

pub struct Ca {
    pub cert_pem: String,
    pub issuer: Issuer<'static, KeyPair>,
}

pub fn new_ca(name: &str) -> Ca {
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

pub fn client_cert(ca: &Ca, name: &str) -> Pem {
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

pub fn der(pem: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .map(|c| c.unwrap())
        .collect()
}

pub fn connector(trust: &[&str], client: Option<&Pem>) -> TlsConnector {
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

pub async fn start(app: App) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

/// One HTTPS request; (status, body)
pub async fn https(
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
