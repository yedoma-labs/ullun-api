//! Streaming request bodies
//!
//! By default a request body is read completely into memory (bounded by
//! `App::max_body_size`) before the handler runs. That is right for JSON, wrong
//! for a 2 GB upload. Routes registered with
//! [`App::stream_body`](crate::app::App::stream_body) skip the buffering: the
//! handler pulls the body piece by piece with [`Request::body_stream`](crate::request::Request::body_stream) and can
//! write it to disk while it arrives, with a limit that is enforced as the
//! bytes come in.
//!
//! ```no_run
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! App::new()
//!     // up to 5 GiB for this prefix; memory use stays at a few chunks
//!     .stream_body("/upload", Some(5 << 30))
//!     .put("/upload/{name}", |mut req: Request| async move {
//!         let name = req.params.get("name")?.to_string();
//!         let mut body = req.body_stream()?;
//!         let bytes = body.save_to(format!("/var/data/{name}.part")).await?;
//!         Ok(Response::json(serde_json::json!({ "stored": bytes })))
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Anything that needs the whole body (`Request::json`, `form`, `multipart`)
//! sees an empty body on a streaming route; choose per prefix. Validate names
//! and paths yourself: the stream only moves bytes.

use crate::error::{Error, Result};
use bytes::Bytes;
use http_body_util::BodyExt;
use std::path::Path;
use tokio::io::{AsyncWrite, AsyncWriteExt};

enum Source {
    Hyper(hyper::body::Incoming),
    Memory(Option<Bytes>),
}

/// A request body that is read in pieces, see the [module documentation](self)
pub struct BodyStream {
    source: Source,
    limit: Option<u64>,
    read: u64,
    declared: Option<u64>,
}

fn too_large(limit: u64) -> Error {
    Error::custom(413, format!("Request body too large (max {limit} bytes)"))
}

impl BodyStream {
    pub(crate) fn from_incoming(body: hyper::body::Incoming, limit: Option<u64>) -> Self {
        use hyper::body::Body;
        let hint = body.size_hint();
        let declared = hint.exact().or_else(|| {
            // a lower bound above zero is only known when Content-Length was sent
            (hint.lower() > 0 && hint.upper() == Some(hint.lower())).then(|| hint.lower())
        });
        Self {
            source: Source::Hyper(body),
            limit,
            read: 0,
            declared,
        }
    }

    pub(crate) fn from_bytes(body: Bytes, limit: Option<u64>) -> Self {
        let declared = Some(body.len() as u64);
        Self {
            source: Source::Memory(Some(body)),
            limit,
            read: 0,
            declared,
        }
    }

    /// Size announced by the client (`Content-Length`), if any. Untrusted; the
    /// limit is checked against what actually arrives.
    pub fn content_length(&self) -> Option<u64> {
        self.declared
    }

    /// Bytes delivered so far
    pub fn bytes_read(&self) -> u64 {
        self.read
    }

    /// The next piece, or `None` at the end. Fails with `413` as soon as the
    /// limit is exceeded, and with `400` when the connection breaks mid-body.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        if let (Some(limit), Some(declared)) = (self.limit, self.declared) {
            if declared > limit {
                return Err(self.refuse(limit).await);
            }
        }
        let next = match &mut self.source {
            Source::Memory(slot) => slot.take().filter(|b| !b.is_empty()),
            Source::Hyper(body) => loop {
                match body.frame().await {
                    None => break None,
                    Some(Err(_)) => return Err(Error::bad_request("Request body was interrupted")),
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            if !data.is_empty() {
                                break Some(data);
                            }
                        }
                    }
                }
            },
        };
        if let Some(data) = &next {
            self.read += data.len() as u64;
            if let Some(limit) = self.limit {
                if self.read > limit {
                    return Err(self.refuse(limit).await);
                }
            }
        }
        Ok(next)
    }

    /// Fail with 413. Part of the rest is read and thrown away first: closing a
    /// connection with unread request data makes the OS reset it, and the
    /// client (notably on Windows) then loses the response.
    async fn refuse(&mut self, limit: u64) -> Error {
        if let Source::Hyper(body) = &mut self.source {
            crate::app::drain(body, self.read).await;
        }
        too_large(limit)
    }

    /// Write the rest of the body to `writer`; returns the bytes copied
    pub async fn copy_to<W: AsyncWrite + Unpin>(&mut self, writer: &mut W) -> Result<u64> {
        let mut total = 0;
        while let Some(chunk) = self.chunk().await? {
            writer
                .write_all(&chunk)
                .await
                .map_err(|e| Error::internal(format!("Writing the upload failed: {e}")))?;
            total += chunk.len() as u64;
        }
        writer
            .flush()
            .await
            .map_err(|e| Error::internal(format!("Writing the upload failed: {e}")))?;
        Ok(total)
    }

    /// Write the rest of the body to a file; returns the bytes written
    ///
    /// The file is created or replaced. If the body is cut off, too large or
    /// the write fails, the partial file is removed. Use a temporary name and
    /// rename it afterwards when readers must never see half-written data.
    pub async fn save_to(&mut self, path: impl AsRef<Path>) -> Result<u64> {
        let path = path.as_ref();
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| Error::internal(format!("Cannot create the upload file: {e}")))?;
        match self.copy_to(&mut file).await {
            Ok(n) => Ok(n),
            Err(e) => {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                Err(e)
            }
        }
    }

    /// Read the rest into memory, still bounded by the limit
    pub async fn collect(&mut self) -> Result<Bytes> {
        let mut out = bytes::BytesMut::new();
        while let Some(chunk) = self.chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out.freeze())
    }
}
