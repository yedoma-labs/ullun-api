//! HTTPS (feature `tls`)
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::tls::TlsConfig;
//!
//! # async fn run() {
//! let tls = TlsConfig::from_pem_files("fullchain.pem", "privkey.pem").unwrap();
//! App::new()
//!     .get("/", |req: Request| async move { Ok(Response::text(format!("secure: {}", req.is_secure()))) })
//!     .tls(tls)
//!     .tls_reload_every(std::time::Duration::from_secs(3600))   // pick up renewed certificates
//!     .run("0.0.0.0:8443")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! * TLS 1.2 and 1.3 with rustls (ring provider); ALPN offers `h2` and `http/1.1`,
//!   so clients that speak HTTP/2 get it automatically.
//! * **Reload without restart**: [`TlsConfig::reload`] re-reads the certificate
//!   files; new connections use the new certificate, open ones keep running. A
//!   renewal that fails to load (half-written file, wrong key) is reported and
//!   the previous certificate stays in service.
//! * **Client certificates** (mutual TLS) with [`TlsConfig::client_auth`],
//!   required or optional; handlers read the presented certificate with
//!   `Request::client_certificate`.
//! * A client that does not complete the handshake within
//!   [`TlsConfig::handshake_timeout`] (default 10 s) is dropped, so half-open
//!   connections cannot pile up.
//!
//! Redirect plain HTTP with [`RequireHttps`](crate::secure::RequireHttps) on a
//! second listener and add [`Hsts`](crate::secure::Hsts) to the HTTPS app.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{RootCertStore, ServerConfig};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

struct Source {
    cert_path: PathBuf,
    key_path: PathBuf,
    modified: Option<(SystemTime, SystemTime)>,
}

struct ClientAuth {
    roots: RootCertStore,
    required: bool,
}

struct State {
    acceptor: TlsAcceptor,
    /// kept so that the configuration can be rebuilt (client auth changes)
    chain_pem: Vec<u8>,
    key_pem: Vec<u8>,
    source: Option<Source>,
}

/// Certificate, key and client-auth policy of an HTTPS server; cheap to clone,
/// clones share the (reloadable) configuration
#[derive(Clone)]
pub struct TlsConfig {
    state: Arc<Mutex<State>>,
    client_auth: Arc<Mutex<Option<Arc<ClientAuth>>>>,
    handshake_timeout: Duration,
}

fn tls_error(what: &str, detail: impl std::fmt::Display) -> Error {
    Error::internal(format!("TLS: {}: {}", what, detail))
}

fn certs_from_pem(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let certs: std::result::Result<Vec<_>, _> = CertificateDer::pem_slice_iter(pem).collect();
    let certs = certs.map_err(|e| tls_error("invalid certificate PEM", e))?;
    if certs.is_empty() {
        return Err(tls_error("no certificate found in the PEM data", "empty"));
    }
    Ok(certs)
}

fn build_server_config(
    chain_pem: &[u8],
    key_pem: &[u8],
    client_auth: Option<&ClientAuth>,
) -> Result<ServerConfig> {
    let chain = certs_from_pem(chain_pem)?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| tls_error("no usable private key in the PEM data", e))?;

    let builder = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| tls_error("protocol versions", e))?;
    let builder = match client_auth {
        None => builder.with_no_client_auth(),
        Some(auth) => {
            let mut verifier = WebPkiClientVerifier::builder_with_provider(
                Arc::new(auth.roots.clone()),
                Arc::new(tokio_rustls::rustls::crypto::ring::default_provider()),
            );
            if !auth.required {
                verifier = verifier.allow_unauthenticated();
            }
            builder.with_client_cert_verifier(
                verifier
                    .build()
                    .map_err(|e| tls_error("client certificate verifier", e))?,
            )
        }
    };
    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|e| tls_error("certificate and key do not match", e))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

impl TlsConfig {
    /// From PEM text: the certificate chain (leaf first) and one private key
    /// (PKCS#8, PKCS#1 or SEC1)
    pub fn from_pem(chain_pem: &[u8], key_pem: &[u8]) -> Result<Self> {
        let config = build_server_config(chain_pem, key_pem, None)?;
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                acceptor: TlsAcceptor::from(Arc::new(config)),
                chain_pem: chain_pem.to_vec(),
                key_pem: key_pem.to_vec(),
                source: None,
            })),
            client_auth: Arc::new(Mutex::new(None)),
            handshake_timeout: Duration::from_secs(10),
        })
    }

    /// From PEM files, which [`reload`](TlsConfig::reload) reads again later
    pub fn from_pem_files(cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        let (cert_path, key_path) = (
            cert_path.as_ref().to_path_buf(),
            key_path.as_ref().to_path_buf(),
        );
        let chain = std::fs::read(&cert_path)
            .map_err(|e| tls_error(&format!("cannot read {}", cert_path.display()), e))?;
        let key = std::fs::read(&key_path)
            .map_err(|e| tls_error(&format!("cannot read {}", key_path.display()), e))?;
        let this = Self::from_pem(&chain, &key)?;
        this.lock().source = Some(Source {
            modified: modified(&cert_path).zip(modified(&key_path)),
            cert_path,
            key_path,
        });
        Ok(this)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn client_auth_policy(&self) -> Option<Arc<ClientAuth>> {
        self.client_auth
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Ask clients for a certificate signed by one of the CAs in `ca_pem`
    ///
    /// With `required`, connections without a valid certificate fail the
    /// handshake. Without it, clients may connect anonymously and handlers
    /// decide per route (`Request::client_certificate` is `None` then);
    /// a certificate that is presented but not trusted is always rejected.
    pub fn client_auth(self, ca_pem: &[u8], required: bool) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        for ca in certs_from_pem(ca_pem)? {
            roots
                .add(ca)
                .map_err(|e| tls_error("unusable CA certificate", e))?;
        }
        let auth = Arc::new(ClientAuth { roots, required });
        // build first: a bad CA must not leave a half-applied policy behind
        let config = {
            let st = self.lock();
            build_server_config(&st.chain_pem, &st.key_pem, Some(&auth))?
        };
        *self.client_auth.lock().unwrap_or_else(|e| e.into_inner()) = Some(auth);
        self.lock().acceptor = TlsAcceptor::from(Arc::new(config));
        Ok(self)
    }

    /// Time allowed for the TLS handshake (default 10 s)
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Read the certificate files again and switch new connections to them
    ///
    /// Returns `Ok(true)` when the files had changed and were applied,
    /// `Ok(false)` when they are unchanged. On error the previous certificate
    /// stays in use.
    pub fn reload(&self) -> Result<bool> {
        let (cert_path, key_path, previous) = {
            let st = self.lock();
            let Some(src) = &st.source else {
                return Ok(false);
            };
            (src.cert_path.clone(), src.key_path.clone(), src.modified)
        };
        let now = modified(&cert_path).zip(modified(&key_path));
        if previous.is_some() && now == previous {
            return Ok(false);
        }
        let chain = std::fs::read(&cert_path)
            .map_err(|e| tls_error(&format!("cannot read {}", cert_path.display()), e))?;
        let key = std::fs::read(&key_path)
            .map_err(|e| tls_error(&format!("cannot read {}", key_path.display()), e))?;
        let auth = self.client_auth_policy();
        let config = build_server_config(&chain, &key, auth.as_deref())?;
        let mut st = self.lock();
        st.acceptor = TlsAcceptor::from(Arc::new(config));
        st.chain_pem = chain;
        st.key_pem = key;
        if let Some(src) = st.source.as_mut() {
            src.modified = now;
        }
        Ok(true)
    }

    /// Perform the handshake; returns the stream, the DER of the client's
    /// leaf certificate if it presented one, and whether the client chose HTTP/2
    pub(crate) async fn accept(
        &self,
        stream: TcpStream,
    ) -> std::io::Result<(TlsStream<TcpStream>, Option<Vec<u8>>, bool)> {
        let acceptor = self.lock().acceptor.clone();
        let tls = tokio::time::timeout(self.handshake_timeout, acceptor.accept(stream))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "TLS handshake timed out")
            })??;
        let cert = tls
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| c.as_ref().to_vec());
        let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2");
        Ok((tls, cert, h2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed() -> (String, String) {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        (ck.cert.pem(), ck.signing_key.serialize_pem())
    }

    #[test]
    fn valid_pem_builds_a_config() {
        let (cert, key) = self_signed();
        assert!(TlsConfig::from_pem(cert.as_bytes(), key.as_bytes()).is_ok());
    }

    #[test]
    fn bad_material_is_reported_not_panicked() {
        let (cert, key) = self_signed();
        let (_, other_key) = self_signed();
        let msg = |r: Result<TlsConfig>| r.err().expect("should fail").message().to_string();

        assert!(msg(TlsConfig::from_pem(b"", key.as_bytes())).contains("no certificate"));
        assert!(msg(TlsConfig::from_pem(b"garbage", key.as_bytes())).contains("TLS"));
        assert!(msg(TlsConfig::from_pem(cert.as_bytes(), b"")).contains("private key"));
        assert!(msg(TlsConfig::from_pem(
            cert.as_bytes(),
            b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n"
        ))
        .contains("TLS"));
        assert!(
            msg(TlsConfig::from_pem(cert.as_bytes(), other_key.as_bytes()))
                .contains("do not match")
        );
        assert!(
            msg(TlsConfig::from_pem_files("/nope/cert.pem", "/nope/key.pem"))
                .contains("cannot read")
        );
    }

    #[test]
    fn reload_applies_changes_and_keeps_the_old_certificate_on_errors() {
        let dir = std::env::temp_dir().join(format!("ullun-tls-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
        let (c1, k1) = self_signed();
        std::fs::write(&cert_path, &c1).unwrap();
        std::fs::write(&key_path, &k1).unwrap();

        let tls = TlsConfig::from_pem_files(&cert_path, &key_path).unwrap();
        assert!(!tls.reload().unwrap(), "unchanged files are not re-read");

        // a renewal arrives (make sure the modification time differs)
        std::thread::sleep(Duration::from_millis(1100));
        let (c2, k2) = self_signed();
        std::fs::write(&cert_path, &c2).unwrap();
        std::fs::write(&key_path, &k2).unwrap();
        assert!(tls.reload().unwrap());
        assert!(!tls.reload().unwrap());

        // a broken renewal is an error and changes nothing
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&cert_path, "half written").unwrap();
        assert!(tls.reload().is_err());
        assert!(tls.reload().is_err(), "still broken, still reported");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configs_from_pem_text_do_not_reload_but_take_client_auth() {
        let (cert, key) = self_signed();
        let tls = TlsConfig::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
        assert!(!tls.reload().unwrap());
        // the CA argument must be a certificate
        assert!(tls.clone().client_auth(b"", true).is_err());
        assert!(tls.clone().client_auth(key.as_bytes(), true).is_err());
        assert!(tls.client_auth(cert.as_bytes(), true).is_ok());
    }
}
