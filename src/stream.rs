//! Streaming response bodies
//!
//! [`Response::stream`](crate::response::Response::stream) hands out a
//! [`BodySender`]; whatever is sent through it goes to the client right away.
//! For Server-Sent Events see [`sse`](crate::sse).

use bytes::Bytes;
use tokio::sync::mpsc;

/// The client closed the connection (or the response was dropped)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disconnected;

impl std::fmt::Display for Disconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("client disconnected")
    }
}

impl std::error::Error for Disconnected {}

/// Sends body chunks of a streaming response; clone it to feed one stream from
/// several tasks. The body ends when all clones are dropped.
#[derive(Clone)]
pub struct BodySender {
    tx: mpsc::Sender<Bytes>,
}

impl BodySender {
    pub(crate) fn new(tx: mpsc::Sender<Bytes>) -> Self {
        Self { tx }
    }

    /// Queue a chunk; waits while the client is slower than the producer
    /// (the channel is bounded), fails once the client is gone.
    pub async fn send(&self, chunk: impl Into<Bytes>) -> Result<(), Disconnected> {
        self.tx.send(chunk.into()).await.map_err(|_| Disconnected)
    }

    /// True once the client has gone away
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Completes when the client has gone away, e.g. to stop a producer that is
    /// waiting for its next event
    pub async fn closed(&self) {
        self.tx.closed().await
    }
}
