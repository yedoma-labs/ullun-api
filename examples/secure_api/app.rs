//! Secure API: application logic, shared by `main.rs` and the tests

use serde_json::json;
use sha2::{Digest, Sha256};
use ullun::observability::{AccessLog, RequestIdLayer};
use ullun::prelude::*;
use ullun::secure::{Hsts, RequireHttps};

pub struct Config {
    /// SHA-256 fingerprints (lower-case hex) of the client certificates that may use /admin
    pub admin_fingerprints: Vec<String>,
}

/// SHA-256 of a DER certificate as lower-case hex: the stable identity of a client
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Admins(Vec<String>);

/// Who is calling: the fingerprint of the verified client certificate, if any
#[derive(Clone)]
struct CallerCert(String);

async fn identify(mut req: Request, next: Next) -> Result<Response> {
    if let Some(der) = req.client_certificate() {
        let fp = fingerprint(der);
        req.extensions.insert(CallerCert(fp));
    }
    next.run(req).await
}

async fn require_admin(req: Request, next: Next) -> Result<Response> {
    let admins = req.state::<Admins>()?;
    match req.extensions.get::<CallerCert>() {
        // the TLS layer proved the client owns a certificate from our CA; the allow-list
        // decides whether that particular certificate is an administrator
        Some(CallerCert(fp)) if admins.0.iter().any(|a| a == fp) => next.run(req).await,
        Some(_) => Err(Error::forbidden(
            "this client certificate is not allowed to administrate",
        )),
        None => Err(Error::unauthorized("a client certificate is required")),
    }
}

/// The HTTPS application (set the TLS configuration with `App::tls` yourself)
pub fn build(config: Config) -> App {
    App::new()
        .state(Admins(
            config
                .admin_fingerprints
                .iter()
                .map(|f| f.to_ascii_lowercase())
                .collect(),
        ))
        .middleware(RequestIdLayer::new())
        .middleware(AccessLog::new().json())
        .middleware(Hsts::new().include_subdomains())
        .middleware(identify)
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(json!({ "status": "ok" })))
        })
        .get("/api/time", |_req: Request| async {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Ok(Response::json(json!({ "unix": now })))
        })
        .get("/api/whoami", |req: Request| async move {
            let admins = req.state::<Admins>()?;
            let fp = req.extensions.get::<CallerCert>().map(|c| c.0.clone());
            Ok(Response::json(json!({
                "secure": req.is_secure(),
                "certificate": fp,
                "admin": fp.as_ref().is_some_and(|f| admins.0.contains(f)),
            })))
        })
        .group("/admin", |g| {
            g.middleware(require_admin)
                .get("/status", |req: Request| async move {
                    let fp = req
                        .extensions
                        .get::<CallerCert>()
                        .map(|c| c.0.clone())
                        .unwrap_or_default();
                    Ok(Response::json(
                        json!({ "status": "all systems nominal", "you": fp }),
                    ))
                })
                .post("/maintenance", |_req: Request| async {
                    Ok(Response::json(json!({ "maintenance": "scheduled" })))
                })
        })
}

/// A listener on the plain HTTP port that only sends people to HTTPS
pub fn redirect_app(https_port: u16) -> App {
    App::new().middleware(RequireHttps::new().https_port(https_port))
}
