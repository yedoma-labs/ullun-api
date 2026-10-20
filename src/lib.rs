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
#[cfg(feature = "compression")]
pub mod compression;
pub mod cookies;
pub mod error;
pub mod handler;
#[cfg(feature = "jwt")]
pub mod jwt;
pub mod middleware;
pub mod openapi;
pub mod rate_limit;
pub mod request;
pub mod response;
pub mod router;
pub mod security;
pub mod static_files;
pub mod template;
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
    pub use crate::template::{TemplateEngine, Templates};
    pub use async_trait::async_trait;
    pub use serde::{Deserialize, Serialize};
}
