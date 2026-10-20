//! ETags and conditional requests
//!
//! * [`ETags`] middleware: tags `GET` responses with an `ETag` derived from the
//!   body and answers `304 Not Modified` when the client's `If-None-Match`
//!   still matches. Responses that already carry an `ETag` keep it.
//! * [`require_if_match`]: optimistic concurrency for updates. The client
//!   sends the `ETag` it last saw as `If-Match`; a stale one gets `412`, a
//!   missing one `428`, so concurrent edits cannot silently overwrite each
//!   other.
//!
//! ```
//! use ullun::conditional::{etag_for, if_none_match_hits};
//!
//! let tag = etag_for(b"hello");
//! assert!(if_none_match_hits(&tag, &tag));
//! assert!(if_none_match_hits("\"other\", *", &tag));
//! assert!(!if_none_match_hits("\"other\"", &tag));
//! ```

use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;

/// Strong ETag for a body: 64-bit FNV-1a plus the length, e.g. `"2f3c...-5"`.
/// Stable across processes and releases (unlike `DefaultHasher`).
pub fn etag_for(body: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in body {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{:016x}-{:x}\"", hash, body.len())
}

/// Entity tags of a header value (`"a", W/"b"`) with the weak marker removed;
/// `None` for `*`
fn tags(header: &str) -> Vec<(bool, &str)> {
    header
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| match t.strip_prefix("W/") {
            Some(rest) => (true, rest),
            None => (false, t),
        })
        .collect()
}

/// `If-None-Match` evaluation: weak comparison, `*` matches anything
pub fn if_none_match_hits(header: &str, current: &str) -> bool {
    let current = current.strip_prefix("W/").unwrap_or(current);
    tags(header).iter().any(|&(_, t)| t == "*" || t == current)
}

/// `If-Match` evaluation: strong comparison, so weak tags never match
pub fn if_match_hits(header: &str, current: &str) -> bool {
    tags(header)
        .iter()
        .any(|&(weak, t)| t == "*" || (!weak && t == current))
}

/// Precondition for updating a resource whose current tag is `current_etag`
///
/// `428 Precondition Required` without an `If-Match` header, `412 Precondition
/// Failed` when it does not match.
pub fn require_if_match(req: &Request, current_etag: &str) -> Result<()> {
    let Some(header) = req.headers.get(http::header::IF_MATCH) else {
        return Err(Error::custom(
            428,
            "This resource requires an If-Match header with its current ETag",
        ));
    };
    let header = header
        .to_str()
        .map_err(|_| Error::bad_request("If-Match is not valid text"))?;
    if if_match_hits(header, current_etag) {
        Ok(())
    } else {
        Err(Error::custom(
            412,
            "The resource changed; fetch it again and retry with the new ETag",
        ))
    }
}

/// Middleware adding `ETag` and `304 Not Modified` handling to `GET` responses
#[derive(Debug, Default, Clone, Copy)]
pub struct ETags;

#[async_trait]
impl Middleware for ETags {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let is_get = req.method == http::Method::GET;
        let if_none_match = req
            .headers
            .get(http::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let mut res = match next.run(req).await {
            Ok(res) => res,
            Err(err) => Response::from_error(err),
        };
        if !is_get || res.status != http::StatusCode::OK || res.is_streaming() {
            return Ok(res);
        }

        let etag = match res
            .headers
            .get(http::header::ETAG)
            .and_then(|v| v.to_str().ok())
        {
            Some(existing) => existing.to_string(),
            None => {
                let tag = etag_for(&res.body);
                res = res.with_header("etag", tag.clone());
                tag
            }
        };

        if if_none_match.is_some_and(|h| if_none_match_hits(&h, &etag)) {
            res.status = http::StatusCode::NOT_MODIFIED;
            res.body = bytes::Bytes::new();
            res.headers.remove(http::header::CONTENT_LENGTH);
            res.headers.remove(http::header::CONTENT_TYPE);
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_is_stable_and_content_dependent() {
        // FNV-1a 64 of "" and "a" are published test vectors
        assert_eq!(etag_for(b""), "\"cbf29ce484222325-0\"");
        assert_eq!(etag_for(b"a"), "\"af63dc4c8601ec8c-1\"");
        assert_ne!(etag_for(b"ab"), etag_for(b"ba"));
        assert_eq!(etag_for(b"same"), etag_for(b"same"));
    }

    #[test]
    fn if_none_match_uses_weak_comparison() {
        let t = "\"abc\"";
        assert!(if_none_match_hits("\"abc\"", t));
        assert!(if_none_match_hits("W/\"abc\"", t));
        assert!(if_none_match_hits("\"x\", W/\"abc\" ,\"y\"", t));
        assert!(if_none_match_hits("*", t));
        assert!(if_none_match_hits("\"abc\"", "W/\"abc\""));
        assert!(!if_none_match_hits("\"abd\"", t));
        assert!(!if_none_match_hits("", t));
        assert!(!if_none_match_hits("abc", t)); // unquoted is not a tag
    }

    #[test]
    fn if_match_uses_strong_comparison() {
        let t = "\"abc\"";
        assert!(if_match_hits("\"abc\"", t));
        assert!(if_match_hits("\"x\", \"abc\"", t));
        assert!(if_match_hits("*", t));
        assert!(!if_match_hits("W/\"abc\"", t));
        assert!(!if_match_hits("\"abd\"", t));
        assert!(!if_match_hits("", t));
    }

    fn req_with(if_match: Option<&str>) -> Request {
        let mut headers = http::HeaderMap::new();
        if let Some(v) = if_match {
            headers.insert(http::header::IF_MATCH, v.parse().unwrap());
        }
        Request::new(
            http::Method::PATCH,
            "/".parse().unwrap(),
            headers,
            bytes::Bytes::new(),
        )
    }

    #[test]
    fn require_if_match_statuses() {
        assert!(require_if_match(&req_with(Some("\"v1\"")), "\"v1\"").is_ok());
        assert_eq!(
            require_if_match(&req_with(None), "\"v1\"")
                .unwrap_err()
                .status_code(),
            428
        );
        assert_eq!(
            require_if_match(&req_with(Some("\"v0\"")), "\"v1\"")
                .unwrap_err()
                .status_code(),
            412
        );
    }
}
