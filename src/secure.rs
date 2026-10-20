//! HTTPS policy: redirect plain HTTP, send HSTS
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::secure::{Hsts, RequireHttps};
//!
//! # async fn run() {
//! // port 80: nothing but a redirect to https
//! let redirect = App::new().middleware(RequireHttps::new().https_port(8443));
//! // port 8443 (TLS configured with App::tls): the real application
//! let app = App::new()
//!     .middleware(Hsts::new())
//!     .get("/", |_req: Request| async { Ok(Response::text("secure")) });
//! # let _ = (redirect, app);
//! # }
//! ```
//!
//! Behind a reverse proxy that terminates TLS the application itself sees
//! plain HTTP: tell both middleware to believe `X-Forwarded-Proto` with
//! `trust_forwarded_proto()`, and only do that when the proxy overwrites the
//! header (a client could otherwise claim to be secure).

use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;

fn forwarded_https(req: &Request) -> bool {
    req.headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
}

/// Split a `Host` header into (host, port), refusing anything that could smuggle
/// characters into a redirect (`evil.com/\r\n`, userinfo, ...)
fn parse_host(raw: &str) -> Option<(String, Option<u16>)> {
    if raw.is_empty() || raw.len() > 255 {
        return None;
    }
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        let valid = !addr.is_empty()
            && addr
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
        if !valid {
            return None;
        }
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?),
        };
        (format!("[{addr}]"), port)
    } else {
        let (host, port) = match raw.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (raw, None),
        };
        let valid = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && !host.starts_with('.')
            && !host.contains("..");
        if !valid {
            return None;
        }
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        None => None,
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => {
            Some(p.parse::<u16>().ok()?)
        }
        Some(_) => return None,
    };
    Some((host, port))
}

/// Redirect (`308`) every plain-HTTP request to the same URL on HTTPS
pub struct RequireHttps {
    https_port: Option<u16>,
    trust_forwarded_proto: bool,
    allowed_hosts: Vec<String>,
}

impl RequireHttps {
    pub fn new() -> Self {
        Self {
            https_port: None,
            trust_forwarded_proto: false,
            allowed_hosts: Vec::new(),
        }
    }

    /// Port of the HTTPS listener when it is not 443
    pub fn https_port(mut self, port: u16) -> Self {
        self.https_port = Some(port).filter(|p| *p != 443);
        self
    }

    /// Treat requests with `X-Forwarded-Proto: https` as secure (reverse proxy)
    pub fn trust_forwarded_proto(mut self) -> Self {
        self.trust_forwarded_proto = true;
        self
    }

    /// Only redirect for these host names (any other `Host` gets `400`), so the
    /// redirect can never point at a host the client made up
    pub fn allowed_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_hosts = hosts
            .into_iter()
            .map(|h| h.into().to_ascii_lowercase())
            .collect();
        self
    }
}

impl Default for RequireHttps {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Middleware for RequireHttps {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        if req.is_secure() || (self.trust_forwarded_proto && forwarded_https(&req)) {
            return next.run(req).await;
        }
        let raw = req
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::bad_request("Missing Host header"))?;
        let (host, _) = parse_host(raw).ok_or_else(|| Error::bad_request("Invalid Host header"))?;
        if !self.allowed_hosts.is_empty()
            && !self
                .allowed_hosts
                .iter()
                .any(|h| *h == host.trim_matches(['[', ']']))
        {
            return Err(Error::bad_request("Unknown host"));
        }
        let port = self.https_port.map(|p| format!(":{p}")).unwrap_or_default();
        let target = req
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .filter(|p| p.starts_with('/'))
            .unwrap_or("/");
        // `//evil.example` would be a protocol-relative URL after the host part
        let target = target.trim_start_matches('/');
        Ok(Response::new(http::StatusCode::PERMANENT_REDIRECT)
            .with_header("location", format!("https://{host}{port}/{target}")))
    }
}

/// `Strict-Transport-Security` on responses to secure requests
///
/// The header is ignored by browsers on plain HTTP, so it is only sent when the
/// request was secure.
pub struct Hsts {
    max_age: u64,
    include_subdomains: bool,
    preload: bool,
    trust_forwarded_proto: bool,
}

impl Hsts {
    /// One year, no subdomains, no preload
    pub fn new() -> Self {
        Self {
            max_age: 31_536_000,
            include_subdomains: false,
            preload: false,
            trust_forwarded_proto: false,
        }
    }

    pub fn max_age(mut self, secs: u64) -> Self {
        self.max_age = secs;
        self
    }

    pub fn include_subdomains(mut self) -> Self {
        self.include_subdomains = true;
        self
    }

    /// Opt in to browser preload lists (hard to undo: read hstspreload.org first)
    pub fn preload(mut self) -> Self {
        self.preload = true;
        self
    }

    pub fn trust_forwarded_proto(mut self) -> Self {
        self.trust_forwarded_proto = true;
        self
    }

    fn value(&self) -> String {
        let mut v = format!("max-age={}", self.max_age);
        if self.include_subdomains {
            v.push_str("; includeSubDomains");
        }
        if self.preload {
            v.push_str("; preload");
        }
        v
    }
}

impl Default for Hsts {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Middleware for Hsts {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let secure = req.is_secure() || (self.trust_forwarded_proto && forwarded_https(&req));
        let res = match next.run(req).await {
            Ok(res) => res,
            Err(err) => Response::from_error(err),
        };
        Ok(if secure {
            res.with_header("strict-transport-security", self.value())
        } else {
            res
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_headers_are_parsed_strictly() {
        let p = |s: &str| parse_host(s);
        assert_eq!(p("example.com"), Some(("example.com".into(), None)));
        assert_eq!(
            p("Example.COM:8080"),
            Some(("example.com".into(), Some(8080)))
        );
        assert_eq!(p("127.0.0.1:80"), Some(("127.0.0.1".into(), Some(80))));
        assert_eq!(p("[::1]"), Some(("[::1]".into(), None)));
        assert_eq!(
            p("[2001:db8::1]:8443"),
            Some(("[2001:db8::1]".into(), Some(8443)))
        );
        for bad in [
            "",
            "evil.com/path",
            "evil.com\r\nX: y",
            "user@evil.com",
            "ev il.com",
            "evil.com:",
            "evil.com:99999",
            "evil.com:80a",
            ".evil.com",
            "a..b",
            "[::1",
            "[]",
            "[zz]",
            "[::1]x",
            "evil.com:80:80",
            "http://evil.com",
            "evil.com\\@good.com",
            &"a".repeat(256),
        ] {
            assert_eq!(p(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hsts_value() {
        assert_eq!(Hsts::new().value(), "max-age=31536000");
        assert_eq!(
            Hsts::new()
                .max_age(60)
                .include_subdomains()
                .preload()
                .value(),
            "max-age=60; includeSubDomains; preload"
        );
    }
}
