//! W3C Trace Context (`traceparent`) propagation
//!
//! A request that crosses several services is only debuggable when every service
//! logs the same trace id. [`TraceLayer`] reads the incoming `traceparent`
//! header (or starts a new trace), gives the request its own span, and makes the
//! ids available to handlers, the access log and outgoing calls:
//!
//! ```no_run
//! use ullun::client::HttpClient;
//! use ullun::prelude::*;
//! use ullun::trace::TraceLayer;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(TraceLayer::new())
//!     .get("/orders", |req: Request| async move {
//!         // the call below carries the same trace id; the callee's parent is this span
//!         let upstream = HttpClient::new()
//!             .get("http://inventory.internal/stock")
//!             .trace(&req)
//!             .send()
//!             .await?;
//!         Ok(Response::text(upstream.text()?))
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! [`AccessLog`](crate::observability::AccessLog) adds `trace=<id>` (text) or
//! `trace_id` (JSON) to its lines when the layer is installed. This is the
//! propagation half of distributed tracing, without an exporter: ship the ids
//! with your logs, or read [`Request::trace`] from your own OpenTelemetry glue.
//! Facing the internet, use [`TraceLayer::ignore_incoming`] so callers cannot
//! choose your trace ids.

use crate::error::Result;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::security::random_bytes;
use async_trait::async_trait;

/// Trace identity of the current request, stored in `Request::extensions`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    /// 32 lowercase hex digits shared by every span of the trace
    pub trace_id: String,
    /// 16 hex digits identifying this service's span
    pub span_id: String,
    /// The caller's span, when the request arrived with a `traceparent`
    pub parent_span_id: Option<String>,
    /// The sampled flag from the caller (new traces are sampled)
    pub sampled: bool,
    /// Vendor state, passed on unchanged
    pub tracestate: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(len: usize) -> Result<String> {
    // all-zero ids are invalid; vanishingly unlikely, but cheap to rule out
    loop {
        let bytes = random_bytes(len)?;
        if bytes.iter().any(|b| *b != 0) {
            return Ok(hex(&bytes));
        }
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl TraceContext {
    /// Parse a `traceparent` header value (`00-<trace>-<span>-<flags>`)
    pub fn parse(header: &str) -> Option<(String, String, bool)> {
        let mut parts = header.trim().split('-');
        let (version, trace, span, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        // version ff is forbidden; future versions may append fields
        if !is_hex(version, 2) || version == "ff" || (version == "00" && parts.next().is_some()) {
            return None;
        }
        if !is_hex(trace, 32) || trace.bytes().all(|b| b == b'0') {
            return None;
        }
        if !is_hex(span, 16) || span.bytes().all(|b| b == b'0') {
            return None;
        }
        if !is_hex(flags, 2) {
            return None;
        }
        let sampled = u8::from_str_radix(flags, 16).ok()? & 1 == 1;
        Some((trace.to_string(), span.to_string(), sampled))
    }

    /// `traceparent` value naming this request's own span
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.trace_id,
            self.span_id,
            if self.sampled { "01" } else { "00" }
        )
    }
}

impl Request {
    /// Trace ids assigned by [`TraceLayer`], if installed
    pub fn trace(&self) -> Option<&TraceContext> {
        self.extensions.get::<TraceContext>()
    }
}

/// Middleware that continues or starts a W3C trace
#[derive(Clone, Debug)]
pub struct TraceLayer {
    trust_incoming: bool,
    echo: bool,
}

impl Default for TraceLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceLayer {
    pub fn new() -> Self {
        Self {
            trust_incoming: true,
            echo: false,
        }
    }

    /// Always start a new trace, whatever the caller sent (public edge)
    pub fn ignore_incoming(mut self) -> Self {
        self.trust_incoming = false;
        self
    }

    /// Return this request's `traceparent` in the response so callers can
    /// find the trace of a request they made
    pub fn echo_response(mut self) -> Self {
        self.echo = true;
        self
    }
}

#[async_trait]
impl Middleware for TraceLayer {
    async fn handle(&self, mut req: Request, next: Next) -> Result<Response> {
        let incoming = self
            .trust_incoming
            .then(|| {
                req.headers
                    .get("traceparent")
                    .and_then(|v| v.to_str().ok())
                    .and_then(TraceContext::parse)
            })
            .flatten();
        let tracestate = incoming
            .as_ref()
            .and_then(|_| req.headers.get("tracestate"))
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 512)
            .map(str::to_string);
        let ctx = match incoming {
            Some((trace_id, parent, sampled)) => TraceContext {
                trace_id,
                span_id: random_hex(8)?,
                parent_span_id: Some(parent),
                sampled,
                tracestate,
            },
            None => TraceContext {
                trace_id: random_hex(16)?,
                span_id: random_hex(8)?,
                parent_span_id: None,
                sampled: true,
                tracestate: None,
            },
        };
        let own = ctx.traceparent();
        req.extensions.insert(ctx);
        let res = next.run(req).await.unwrap_or_else(Response::from_error);
        Ok(if self.echo {
            res.with_header("traceparent", own)
        } else {
            res
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const SPAN: &str = "00f067aa0ba902b7";

    #[test]
    fn parses_valid_headers() {
        let (t, s, sampled) = TraceContext::parse(&format!("00-{TRACE}-{SPAN}-01")).unwrap();
        assert_eq!((t.as_str(), s.as_str(), sampled), (TRACE, SPAN, true));
        let (_, _, sampled) = TraceContext::parse(&format!("00-{TRACE}-{SPAN}-00")).unwrap();
        assert!(!sampled);
        // a future version may carry extra fields
        assert!(TraceContext::parse(&format!("01-{TRACE}-{SPAN}-01-extra")).is_some());
    }

    #[test]
    fn rejects_malformed_headers() {
        for bad in [
            "".to_string(),
            "garbage".to_string(),
            format!("ff-{TRACE}-{SPAN}-01"),
            format!("00-{TRACE}-{SPAN}"),
            format!("00-{TRACE}-{SPAN}-01-extra"),
            format!("00-{}-{SPAN}-01", "0".repeat(32)),
            format!("00-{TRACE}-{}-01", "0".repeat(16)),
            format!("00-{}-{SPAN}-01", TRACE.to_uppercase()),
            format!("00-{}-{SPAN}-01", &TRACE[..30]),
            format!("00-{TRACE}-{SPAN}-0g"),
        ] {
            assert!(TraceContext::parse(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn own_traceparent_names_the_span() {
        let ctx = TraceContext {
            trace_id: TRACE.into(),
            span_id: SPAN.into(),
            parent_span_id: None,
            sampled: true,
            tracestate: None,
        };
        assert_eq!(ctx.traceparent(), format!("00-{TRACE}-{SPAN}-01"));
        let unsampled = TraceContext {
            sampled: false,
            ..ctx
        };
        assert_eq!(unsampled.traceparent(), format!("00-{TRACE}-{SPAN}-00"));
    }
}
