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

pub mod app;
pub mod cookies;
pub mod error;
pub mod handler;
pub mod middleware;
pub mod request;
pub mod response;
pub mod router;
pub mod static_files;

pub mod prelude {
    pub use crate::app::{App, RouteGroup};
    pub use crate::cookies::{Cookie, Cookies};
    pub use crate::error::{Error, Result};
    pub use crate::handler::Handler;
    pub use crate::middleware::{Middleware, Next};
    pub use crate::request::{Params, Query, Request};
    pub use crate::response::{Json, Response};
    pub use crate::router::Router;
    pub use async_trait::async_trait;
    pub use serde::{Deserialize, Serialize};
}
