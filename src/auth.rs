//! API-key and HTTP Basic authentication middleware
//!
//! Small, dependency-free authentication for service-to-service calls, admin
//! endpoints and internal tools. Secrets are compared in constant time against
//! every configured entry (no early exit that would show which key was close),
//! failures answer `401` with `WWW-Authenticate`, and the name of the
//! authenticated caller is stored for handlers:
//!
//! ```no_run
//! use ullun::auth::{ApiKeyAuth, BasicAuth};
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! App::new()
//!     .group("/internal", |g| {
//!         g.middleware(ApiKeyAuth::new().key("billing-service", "k_live_9c1e7a"))
//!             .get("/who", |req: Request| async move {
//!                 Ok(Response::text(req.principal().unwrap_or("nobody").to_string()))
//!             })
//!     })
//!     .group("/admin", |g| {
//!         g.middleware(BasicAuth::new("admin area").user("ops", "correct horse battery staple"))
//!             .get("/", |_req: Request| async { Ok(Response::text("welcome")) })
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Keys and passwords live in memory as given: load them from the environment
//! or a secret store, never hard-code real ones. Basic credentials are only
//! protected by TLS. For per-user passwords stored in a database use
//! [`BasicAuth::verify_with`] and a password hash (argon2, bcrypt) of your choice.
//! Failed attempts are not rate limited here: put a
//! [`RateLimiter`](crate::rate_limit::RateLimiter) in front.

use crate::error::Result;
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::security::constant_time_eq;
use async_trait::async_trait;
use std::sync::Arc;

/// The authenticated caller, stored in `Request::extensions`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal(pub String);

impl Request {
    /// Name of the caller authenticated by [`ApiKeyAuth`] or [`BasicAuth`]
    pub fn principal(&self) -> Option<&str> {
        self.extensions.get::<Principal>().map(|p| p.0.as_str())
    }
}

fn unauthorized(challenge: &str, message: &str) -> Response {
    Response::from_error(crate::error::Error::unauthorized(message))
        .with_header("www-authenticate", challenge)
        .with_header("cache-control", "no-store")
}

fn exempt(paths: &[String], req: &Request) -> bool {
    paths.iter().any(|p| p == req.uri.path())
}

// ---- API keys ----

/// Authenticate callers by a secret in a request header
///
/// Several keys can be active at once (one per caller, or old and new during a
/// rotation); the matching entry's name becomes the [`Principal`].
#[derive(Clone)]
pub struct ApiKeyAuth {
    header: String,
    keys: Vec<(String, String)>,
    bearer: bool,
    except: Vec<String>,
}

impl Default for ApiKeyAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiKeyAuth {
    /// Reads the key from `X-API-Key`
    pub fn new() -> Self {
        Self {
            header: "x-api-key".into(),
            keys: Vec::new(),
            bearer: false,
            except: Vec::new(),
        }
    }

    /// Header carrying the key
    pub fn header(mut self, name: &str) -> Self {
        self.header = name.to_ascii_lowercase();
        self
    }

    /// Also accept the key as `Authorization: Bearer <key>`
    pub fn allow_bearer(mut self) -> Self {
        self.bearer = true;
        self
    }

    /// Accept `secret` and report its owner as `name`
    pub fn key(mut self, name: &str, secret: &str) -> Self {
        self.keys.push((name.to_string(), secret.to_string()));
        self
    }

    /// Paths served without a key (health checks)
    pub fn except_paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.except = paths.into_iter().map(Into::into).collect();
        self
    }

    fn lookup(&self, presented: &str) -> Option<&str> {
        // visit every entry so the time does not depend on which one matched
        let mut found = None;
        for (name, secret) in &self.keys {
            if constant_time_eq(secret.as_bytes(), presented.as_bytes()) && found.is_none() {
                found = Some(name.as_str());
            }
        }
        found
    }
}

#[async_trait]
impl Middleware for ApiKeyAuth {
    async fn handle(&self, mut req: Request, next: Next) -> Result<Response> {
        if exempt(&self.except, &req) {
            return next.run(req).await;
        }
        let presented = req
            .headers
            .get(self.header.as_str())
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| {
                self.bearer
                    .then(|| {
                        req.headers
                            .get(http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("Bearer "))
                            .map(str::to_string)
                    })
                    .flatten()
            });
        let challenge = if self.bearer { "Bearer" } else { "ApiKey" };
        let Some(presented) = presented else {
            return Ok(unauthorized(challenge, "API key required"));
        };
        match self.lookup(&presented) {
            Some(name) => {
                req.extensions.insert(Principal(name.to_string()));
                next.run(req).await
            }
            None => Ok(unauthorized(challenge, "Invalid API key")),
        }
    }
}

// ---- Basic ----

type Verifier = dyn Fn(String, String) -> BoxFuture<'static, bool> + Send + Sync;

/// HTTP Basic authentication (RFC 7617)
#[derive(Clone)]
pub struct BasicAuth {
    realm: String,
    users: Vec<(String, String)>,
    verifier: Option<Arc<Verifier>>,
    except: Vec<String>,
}

impl BasicAuth {
    /// `realm` is shown in the browser's login dialog
    pub fn new(realm: &str) -> Self {
        Self {
            realm: realm.replace(['"', '\\', '\r', '\n'], ""),
            users: Vec::new(),
            verifier: None,
            except: Vec::new(),
        }
    }

    /// Accept this user and password
    pub fn user(mut self, name: &str, password: &str) -> Self {
        self.users.push((name.to_string(), password.to_string()));
        self
    }

    /// Check credentials yourself, e.g. against password hashes in a database.
    /// Replaces the built-in users.
    pub fn verify_with<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(String, String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = bool> + Send + 'static,
    {
        self.verifier = Some(Arc::new(move |user, pass| Box::pin(f(user, pass))));
        self
    }

    /// Paths served without credentials
    pub fn except_paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.except = paths.into_iter().map(Into::into).collect();
        self
    }

    fn challenge(&self) -> String {
        format!("Basic realm=\"{}\", charset=\"UTF-8\"", self.realm)
    }

    fn check_builtin(&self, user: &str, password: &str) -> bool {
        // every entry is compared, name and password both, without early exit
        let mut ok = false;
        for (name, secret) in &self.users {
            let name_ok = constant_time_eq(name.as_bytes(), user.as_bytes());
            let pass_ok = constant_time_eq(secret.as_bytes(), password.as_bytes());
            ok |= name_ok & pass_ok;
        }
        ok
    }
}

/// Decode standard or URL-safe base64 with optional padding
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in input.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[async_trait]
impl Middleware for BasicAuth {
    async fn handle(&self, mut req: Request, next: Next) -> Result<Response> {
        if exempt(&self.except, &req) {
            return next.run(req).await;
        }
        let credentials = req
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let (scheme, rest) = v.split_once(' ')?;
                scheme.eq_ignore_ascii_case("basic").then_some(rest.trim())
            })
            .and_then(base64_decode)
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|text| {
                let (user, pass) = text.split_once(':')?;
                Some((user.to_string(), pass.to_string()))
            });
        let Some((user, pass)) = credentials else {
            return Ok(unauthorized(&self.challenge(), "Credentials required"));
        };
        let ok = match &self.verifier {
            Some(verify) => verify(user.clone(), pass).await,
            None => self.check_builtin(&user, &pass),
        };
        if !ok {
            return Ok(unauthorized(&self.challenge(), "Invalid credentials"));
        }
        req.extensions.insert(Principal(user));
        next.run(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decoding() {
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
        assert_eq!(base64_decode("dXNlcjpwYXNzMQ==").unwrap(), b"user:pass1");
        assert_eq!(base64_decode("dXNlcjpwYXNzMQ").unwrap(), b"user:pass1");
        // url-safe alphabet
        assert_eq!(base64_decode("_-8").unwrap(), vec![0xff, 0xef]);
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
        assert!(base64_decode("not base64!").is_none());
    }

    #[test]
    fn realm_cannot_break_the_header() {
        let auth = BasicAuth::new("a\"b\r\nX-Evil: 1");
        assert_eq!(
            auth.challenge(),
            "Basic realm=\"abX-Evil: 1\", charset=\"UTF-8\""
        );
    }

    #[test]
    fn api_key_lookup_returns_the_owner() {
        let a = ApiKeyAuth::new().key("a", "secret-a").key("b", "secret-b");
        assert_eq!(a.lookup("secret-b"), Some("b"));
        assert_eq!(a.lookup("secret-"), None);
        assert_eq!(a.lookup(""), None);
    }
}
