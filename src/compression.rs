//! Gzip response compression middleware (feature `compression`)
//!
//! Compresses text-like responses (`text/*`, JSON, JavaScript, XML, SVG) of at
//! least [`Compression::min_size`] bytes when the client sends
//! `Accept-Encoding: gzip`. Already encoded responses, `HEAD` requests and
//! bodiless statuses are left alone. `Vary: Accept-Encoding` is added whenever a
//! response could have been compressed, so caches keep both variants apart.
//!
//! ```no_run
//! use ullun::compression::Compression;
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(Compression::new())
//!     .get("/", |_req: Request| async { Ok(Response::text("hello ".repeat(500))) })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```

use crate::error::Result;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use bytes::Bytes;
use flate2::write::GzEncoder;
use flate2::Compression as Level;
use std::io::Write;

/// Gzip compression middleware
pub struct Compression {
    min_size: usize,
    level: u32,
}

impl Compression {
    /// Defaults: bodies of 1 KiB and more, compression level 6
    pub fn new() -> Self {
        Self {
            min_size: 1024,
            level: 6,
        }
    }

    /// Smallest body (bytes) worth compressing
    pub fn min_size(mut self, bytes: usize) -> Self {
        self.min_size = bytes;
        self
    }

    /// gzip level 0 (none) to 9 (best); values above 9 are clamped
    pub fn level(mut self, level: u32) -> Self {
        self.level = level.min(9);
        self
    }
}

impl Default for Compression {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Middleware for Compression {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let wants_gzip = accepts_gzip(&req);
        let is_head = req.method == http::Method::HEAD;
        let mut res = match next.run(req).await {
            Ok(res) => res,
            Err(err) => Response::from_error(err),
        };

        if is_head
            || res.status.is_informational()
            || res.status == http::StatusCode::NO_CONTENT
            || res.status == http::StatusCode::NOT_MODIFIED
            || res.headers.contains_key(http::header::CONTENT_ENCODING)
            || !is_compressible(&res)
        {
            return Ok(res);
        }

        add_vary(&mut res);
        if !wants_gzip || res.body.len() < self.min_size {
            return Ok(res);
        }

        let mut encoder = GzEncoder::new(
            Vec::with_capacity(res.body.len() / 2),
            Level::new(self.level),
        );
        if encoder.write_all(&res.body).is_err() {
            return Ok(res);
        }
        let Ok(compressed) = encoder.finish() else {
            return Ok(res);
        };
        if compressed.len() >= res.body.len() {
            return Ok(res);
        }

        res.body = Bytes::from(compressed);
        res.headers.insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static("gzip"),
        );
        res.headers.remove(http::header::CONTENT_LENGTH);
        Ok(res)
    }
}

/// `gzip` (or `*`) listed with a non-zero q-value
fn accepts_gzip(req: &Request) -> bool {
    let Some(value) = req
        .headers
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    value.split(',').any(|item| {
        let mut parts = item.split(';');
        let coding = parts.next().unwrap_or("").trim();
        if !(coding.eq_ignore_ascii_case("gzip") || coding == "*") {
            return false;
        }
        parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .next()
            .and_then(|q| q.trim().parse::<f32>().ok())
            .map(|q| q > 0.0)
            .unwrap_or(true)
    })
}

fn is_compressible(res: &Response) -> bool {
    let Some(ct) = res
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let ct = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.ends_with("json")
        || ct.ends_with("javascript")
        || ct.ends_with("xml")
        || ct == "image/svg+xml"
}

fn add_vary(res: &mut Response) {
    let existing = res
        .headers
        .get_all(http::header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ");
    if existing.split(',').any(|v| {
        let v = v.trim();
        v == "*" || v.eq_ignore_ascii_case("accept-encoding")
    }) {
        return;
    }
    let value = if existing.is_empty() {
        "Accept-Encoding".to_string()
    } else {
        format!("{}, Accept-Encoding", existing)
    };
    if let Ok(v) = http::HeaderValue::from_str(&value) {
        res.headers.insert(http::header::VARY, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with(accept: Option<&str>) -> Request {
        let mut headers = http::HeaderMap::new();
        if let Some(a) = accept {
            headers.insert(http::header::ACCEPT_ENCODING, a.parse().unwrap());
        }
        Request::new(
            http::Method::GET,
            "/".parse().unwrap(),
            headers,
            Bytes::new(),
        )
    }

    #[test]
    fn accept_encoding_parsing() {
        assert!(accepts_gzip(&req_with(Some("gzip"))));
        assert!(accepts_gzip(&req_with(Some("br, GZIP;q=0.5"))));
        assert!(accepts_gzip(&req_with(Some("*"))));
        assert!(!accepts_gzip(&req_with(Some("gzip;q=0"))));
        assert!(!accepts_gzip(&req_with(Some("br, deflate"))));
        assert!(!accepts_gzip(&req_with(None)));
    }

    #[test]
    fn vary_is_merged_not_duplicated() {
        let mut res = Response::text("x");
        add_vary(&mut res);
        assert_eq!(res.headers["vary"], "Accept-Encoding");
        add_vary(&mut res);
        assert_eq!(res.headers.get_all("vary").iter().count(), 1);

        let mut res = Response::text("x").with_header("vary", "Origin");
        add_vary(&mut res);
        assert_eq!(res.headers["vary"], "Origin, Accept-Encoding");
    }

    #[test]
    fn compressible_types() {
        let t = |ct: &str| is_compressible(&Response::ok().with_header("content-type", ct));
        assert!(t("text/html; charset=utf-8") && t("application/json") && t("image/svg+xml"));
        assert!(t("application/javascript") && t("application/ld+json") && t("application/xml"));
        assert!(!t("image/png") && !t("application/zip") && !t("application/octet-stream"));
        assert!(!is_compressible(&Response::ok()));
    }
}
