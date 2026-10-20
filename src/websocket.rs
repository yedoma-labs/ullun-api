//! WebSocket endpoints (feature `websocket`)
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::websocket::{Message, WebSocket};
//!
//! # async fn run() {
//! App::new()
//!     .ws("/echo/{room}", |req: Request, mut socket: WebSocket| async move {
//!         let room = req.params.get_opt("room").unwrap_or("lobby").to_string();
//!         let _ = socket.send_text(format!("welcome to {room}")).await;
//!         while let Some(Ok(msg)) = socket.recv().await {
//!             match msg {
//!                 Message::Text(t) => { let _ = socket.send_text(t).await; }
//!                 Message::Close => break,
//!                 _ => {}
//!             }
//!         }
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! The handshake is validated (version 13, key, `Connection`/`Upgrade`) and,
//! because browsers do not apply the same-origin policy to WebSockets, a
//! cross-site `Origin` is rejected with `403` unless it is listed in
//! [`WsOptions::allowed_origins`]. Clients without an `Origin` header (native
//! apps, CLI tools) are accepted; authenticate them in middleware.

use crate::error::{Error, Result};
use crate::handler::Handler;
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};
use tokio_tungstenite::WebSocketStream;

/// A WebSocket message
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    /// Pings are answered automatically; they are surfaced for information
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    /// The peer started the closing handshake
    Close,
}

/// Per-endpoint settings
#[derive(Debug, Clone)]
pub struct WsOptions {
    /// Largest accepted message and frame in bytes (default 1 MiB)
    pub max_message_size: usize,
    /// Extra `Origin` values (e.g. `https://app.example.com`) that may connect
    /// besides the host serving the endpoint
    pub allowed_origins: Vec<String>,
}

impl Default for WsOptions {
    fn default() -> Self {
        Self {
            max_message_size: 1024 * 1024,
            allowed_origins: Vec::new(),
        }
    }
}

/// An upgraded connection
pub struct WebSocket {
    inner: WebSocketStream<TokioIo<Upgraded>>,
}

impl WebSocket {
    /// Next message, or `None` once the connection is closed
    pub async fn recv(&mut self) -> Option<Result<Message>> {
        loop {
            return match self.inner.next().await? {
                Ok(WsMessage::Text(t)) => Some(Ok(Message::Text(t.as_str().to_owned()))),
                Ok(WsMessage::Binary(b)) => Some(Ok(Message::Binary(b.to_vec()))),
                Ok(WsMessage::Ping(b)) => Some(Ok(Message::Ping(b.to_vec()))),
                Ok(WsMessage::Pong(b)) => Some(Ok(Message::Pong(b.to_vec()))),
                Ok(WsMessage::Close(_)) => Some(Ok(Message::Close)),
                Ok(WsMessage::Frame(_)) => continue,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    None
                }
                Err(e) => Some(Err(Error::bad_request(format!("WebSocket: {}", e)))),
            };
        }
    }

    pub async fn send(&mut self, message: Message) -> Result<()> {
        let msg = match message {
            Message::Text(t) => WsMessage::text(t),
            Message::Binary(b) => WsMessage::binary(b),
            Message::Ping(b) => WsMessage::Ping(b.into()),
            Message::Pong(b) => WsMessage::Pong(b.into()),
            Message::Close => WsMessage::Close(None),
        };
        self.inner
            .send(msg)
            .await
            .map_err(|e| Error::internal(format!("WebSocket send: {}", e)))
    }

    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::Text(text.into())).await
    }

    pub async fn send_binary(&mut self, data: impl Into<Vec<u8>>) -> Result<()> {
        self.send(Message::Binary(data.into())).await
    }

    /// Start the closing handshake and flush it
    pub async fn close(&mut self) -> Result<()> {
        self.inner
            .close(None)
            .await
            .map_err(|e| Error::internal(format!("WebSocket close: {}", e)))
    }
}

/// Route handler performing the handshake; created by `App::ws`
pub(crate) struct WsRoute<F> {
    handler: Arc<F>,
    options: WsOptions,
}

impl<F> WsRoute<F> {
    pub(crate) fn new(handler: F, options: WsOptions) -> Self {
        Self {
            handler: Arc::new(handler),
            options,
        }
    }
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers.get(name).and_then(|v| v.to_str().ok())
}

/// Token in a comma separated header, across repeated header lines
fn has_token(req: &Request, name: &str, token: &str) -> bool {
    req.headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
}

/// Same host as the request, or explicitly allowed
fn origin_allowed(req: &Request, options: &WsOptions) -> bool {
    let Some(origin) = header(req, "origin") else {
        return true;
    };
    if options.allowed_origins.iter().any(|o| o == origin) {
        return true;
    }
    let authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    header(req, "host")
        .map(|h| h.eq_ignore_ascii_case(authority))
        .unwrap_or(false)
}

#[async_trait]
impl<F, Fut> Handler for WsRoute<F>
where
    F: Fn(Request, WebSocket) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    async fn call(&self, mut req: Request) -> Result<Response> {
        if !has_token(&req, "connection", "upgrade")
            || !header(&req, "upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            return Err(Error::bad_request("Expected a WebSocket upgrade request"));
        }
        if header(&req, "sec-websocket-version") != Some("13") {
            return Ok(
                Response::from_error(Error::custom(426, "Unsupported WebSocket version"))
                    .with_header("sec-websocket-version", "13"),
            );
        }
        let Some(key) = header(&req, "sec-websocket-key").filter(|k| !k.is_empty()) else {
            return Err(Error::bad_request("Missing Sec-WebSocket-Key"));
        };
        let accept = derive_accept_key(key.as_bytes());
        if !origin_allowed(&req, &self.options) {
            return Err(Error::forbidden("Origin not allowed"));
        }
        let Some(on_upgrade) = req.extensions.remove::<OnUpgrade>() else {
            return Err(Error::internal("Connection cannot be upgraded"));
        };

        let handler = self.handler.clone();
        let config = WebSocketConfig::default()
            .max_message_size(Some(self.options.max_message_size))
            .max_frame_size(Some(self.options.max_message_size));
        tokio::spawn(async move {
            let Ok(upgraded) = on_upgrade.await else {
                return;
            };
            let inner = WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                Some(config),
            )
            .await;
            handler(req, WebSocket { inner }).await;
        });

        let mut res = Response::new(http::StatusCode::SWITCHING_PROTOCOLS)
            .with_header("upgrade", "websocket")
            .with_header("connection", "Upgrade")
            .with_header("sec-websocket-accept", accept);
        res.body = bytes::Bytes::new();
        Ok(res)
    }
}
