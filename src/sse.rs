//! Server-Sent Events
//!
//! ```
//! use ullun::prelude::*;
//! use ullun::sse::{sse, Event};
//!
//! async fn ticker(_req: Request) -> Result<Response> {
//!     let (tx, res) = sse();
//!     tokio::spawn(async move {
//!         for n in 1..=3 {
//!             let event = Event::data(format!("tick {n}")).event("tick").id(n.to_string());
//!             if tx.send(event).await.is_err() {
//!                 return; // browser closed the tab
//!             }
//!             tokio::time::sleep(std::time::Duration::from_secs(1)).await;
//!         }
//!     });
//!     Ok(res)
//! }
//! ```
//!
//! Browser side: `new EventSource("/ticker").addEventListener("tick", e => ...)`.
//! Newlines in field values cannot inject extra fields: `event` and `id` have
//! them removed, `data` is split into one `data:` line per line.

use crate::error::{Error, Result};
use crate::response::Response;
use crate::stream::{BodySender, Disconnected};
use serde::Serialize;

/// One server-sent event
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    event: Option<String>,
    id: Option<String>,
    retry_ms: Option<u64>,
    data: Option<String>,
    comment: Option<String>,
}

impl Event {
    /// An event carrying `data`
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            data: Some(data.into()),
            ..Self::default()
        }
    }

    /// An event whose data is `value` serialized as JSON
    pub fn json<T: Serialize>(value: &T) -> Result<Self> {
        let data = serde_json::to_string(value)
            .map_err(|e| Error::internal(format!("SSE JSON: {}", e)))?;
        Ok(Self::data(data))
    }

    /// A comment line, ignored by `EventSource`; useful as a keep-alive
    pub fn comment(text: impl Into<String>) -> Self {
        Self {
            comment: Some(text.into()),
            ..Self::default()
        }
    }

    /// Event name, delivered to `addEventListener(name, ...)`
    pub fn event(mut self, name: impl Into<String>) -> Self {
        self.event = Some(name.into());
        self
    }

    /// Event id; the browser sends the last one back as `Last-Event-ID` on reconnect
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Reconnect delay hint in milliseconds
    pub fn retry(mut self, millis: u64) -> Self {
        self.retry_ms = Some(millis);
        self
    }

    /// Wire format, terminated by a blank line
    pub fn encode(&self) -> String {
        fn single_line(s: &str) -> String {
            s.chars().filter(|c| *c != '\n' && *c != '\r').collect()
        }
        let mut out = String::new();
        if let Some(c) = &self.comment {
            for line in c.lines() {
                out.push_str(": ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if let Some(e) = &self.event {
            out.push_str(&format!("event: {}\n", single_line(e)));
        }
        if let Some(id) = &self.id {
            out.push_str(&format!("id: {}\n", single_line(id)));
        }
        if let Some(ms) = self.retry_ms {
            out.push_str(&format!("retry: {}\n", ms));
        }
        if let Some(d) = &self.data {
            // `lines()` splits on \n and \r\n; an empty payload is one empty line
            let mut any = false;
            for line in d.split('\n') {
                any = true;
                out.push_str("data: ");
                out.push_str(line.trim_end_matches('\r'));
                out.push('\n');
            }
            debug_assert!(any);
        }
        out.push('\n');
        out
    }
}

/// Sends events of an SSE response
#[derive(Clone)]
pub struct SseSender {
    inner: BodySender,
}

impl SseSender {
    pub async fn send(&self, event: Event) -> std::result::Result<(), Disconnected> {
        self.inner.send(event.encode()).await
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Completes when the browser has disconnected
    pub async fn closed(&self) {
        self.inner.closed().await
    }
}

/// An SSE response with the right headers (`text/event-stream`, no caching,
/// no proxy buffering) and the sender for its events
pub fn sse() -> (SseSender, Response) {
    let (tx, res) = Response::stream();
    let res = res
        .with_header("content-type", "text/event-stream; charset=utf-8")
        .with_header("cache-control", "no-cache")
        .with_header("x-accel-buffering", "no");
    (SseSender { inner: tx }, res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_fields_in_order_and_ends_with_a_blank_line() {
        let e = Event::data("hello").event("greeting").id("7").retry(1500);
        assert_eq!(
            e.encode(),
            "event: greeting\nid: 7\nretry: 1500\ndata: hello\n\n"
        );
    }

    #[test]
    fn multiline_data_becomes_multiple_data_lines() {
        assert_eq!(
            Event::data("a\nb\r\nc").encode(),
            "data: a\ndata: b\ndata: c\n\n"
        );
        assert_eq!(Event::data("").encode(), "data: \n\n");
        assert_eq!(
            Event::data("trailing\n").encode(),
            "data: trailing\ndata: \n\n"
        );
    }

    #[test]
    fn newlines_cannot_inject_fields() {
        let e = Event::data("x").event("a\nretry: 1").id("1\r\ndata: evil");
        let wire = e.encode();
        assert_eq!(wire, "event: aretry: 1\nid: 1data: evil\ndata: x\n\n");
        assert_eq!(wire.matches("\n\n").count(), 1);
    }

    #[test]
    fn comments_and_json() {
        assert_eq!(Event::comment("keep-alive").encode(), ": keep-alive\n\n");
        assert_eq!(Event::comment("a\nb").encode(), ": a\n: b\n\n");
        let e = Event::json(&serde_json::json!({ "up": true })).unwrap();
        assert_eq!(e.encode(), "data: {\"up\":true}\n\n");
    }
}
