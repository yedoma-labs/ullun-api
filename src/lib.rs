//! # ullun-api
//!
//! Express.js-inspired web framework for Rust.
//!
//! ## Quick Start
//!
//! ```no_run
//! use ullun::prelude::*;
//!
//! #[tokio::main]
//! async fn main() {
//!     App::new()
//!         .get_with_params("/hello/{name}", |params: Params| async move {
//!             Ok(Response::json(serde_json::json!({
//!                 "message": format!("Hello, {}!", params.get("name").unwrap())
//!             })))
//!         })
//!         .run("127.0.0.1:3000")
//!         .await
//!         .unwrap();
//! }
//! ```

//! ## Optional features
//!
//! | Feature | Adds |
//! |---------|------|
//! | `jwt` | `ullun::jwt` - HS256 tokens and bearer-auth middleware |
//! | `compression` | `ullun::compression` - gzip middleware |
//! | `websocket` | `App::ws` and `ullun::websocket` |
//! | `full` | all of the above |
//!
//! Always available: [`rate_limit`], [`openapi`], [`template`], form parsing
//! (`Request::form`) and group-level middleware.

pub mod app;
pub mod cache;
pub mod client;
#[cfg(feature = "compression")]
pub mod compression;
pub mod conditional;
pub mod cookies;
pub mod cors;
pub mod csrf;
pub mod error;
pub mod files;
pub mod handler;
pub mod health;
pub mod hub;
pub mod idempotency;
pub mod jobs;
#[cfg(feature = "jwt")]
pub mod jwt;
pub mod limits;
pub mod middleware;
pub mod multipart;
pub mod negotiate;
pub mod observability;
pub mod openapi;
pub mod problem;
pub mod proxy;
pub mod rate_limit;
pub mod request;
pub mod response;
pub mod router;
pub mod secure;
pub mod security;
pub mod security_headers;
pub mod session;
pub mod shutdown;
#[cfg(feature = "signing")]
pub mod signed_cookies;
pub mod sse;
pub mod static_files;
pub mod stream;
pub mod template;
pub mod testing;
#[cfg(feature = "tls")]
pub mod tls;
pub mod trace;
pub mod upload;
pub mod validate;
#[cfg(feature = "websocket")]
pub mod websocket;

pub mod prelude {
    pub use crate::app::{App, RouteGroup};
    pub use crate::cookies::{Cookie, Cookies};
    pub use crate::error::{Error, Result};
    pub use crate::handler::Handler;
    pub use crate::middleware::{Middleware, Next};
    pub use crate::request::{Params, Query, Request};
    pub use crate::response::{Json, Response};
    pub use crate::router::Router;
    pub use crate::shutdown::{Context, Shutdown};
    pub use crate::template::{TemplateEngine, Templates};
    pub use async_trait::async_trait;
    pub use serde::{Deserialize, Serialize};
}
