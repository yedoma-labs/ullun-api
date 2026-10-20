//! CSRF protection for cookie-authenticated forms and APIs
//!
//! [`Csrf`] needs the [`SessionLayer`](crate::session::SessionLayer) before it.
//! Every request with an unsafe method (`POST`, `PUT`, `PATCH`, `DELETE`) must
//! prove it was made by your own pages in two ways:
//!
//! 1. **Origin check**: an `Origin` header (or, without it, `Sec-Fetch-Site`) that
//!    points to another site is rejected with `403`.
//! 2. **Token**: the session's token, sent as the `x-csrf-token` header or as the
//!    `csrf_token` field of an urlencoded or multipart form. Compared in
//!    constant time.
//!
//! ```no_run
//! use ullun::csrf::Csrf;
//! use ullun::prelude::*;
//! use ullun::session::SessionLayer;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(SessionLayer::new())
//!     .middleware(Csrf::new())
//!     .get("/form", |req: Request| async move {
//!         let token = req.session()?.csrf_token()?;
//!         Ok(Response::html(format!(
//!             r#"<form method="post" action="/save"><input type="hidden" name="csrf_token" value="{token}"><button>Save</button></form>"#
//!         )))
//!     })
//!     .post("/save", |_req: Request| async { Ok(Response::text("saved")) })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Scripts read the token from a `<meta>` tag you render and send it as a
//! header. Requests that authenticate with an `Authorization` header instead of
//! cookies are not CSRF-prone: exempt them with [`Csrf::exempt`].

use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::security::constant_time_eq;
use async_trait::async_trait;
use std::sync::Arc;

type Exempt = dyn Fn(&Request) -> bool + Send + Sync;

/// CSRF middleware, see the [module docs](self)
pub struct Csrf {
    header: String,
    field: String,
    allowed_origins: Vec<String>,
    exempt: Option<Arc<Exempt>>,
}

impl Csrf {
    pub fn new() -> Self {
        Self {
            header: "x-csrf-token".into(),
            field: "csrf_token".into(),
            allowed_origins: Vec::new(),
            exempt: None,
        }
    }

    /// Name of the header carrying the token (default `x-csrf-token`)
    pub fn header_name(mut self, name: impl Into<String>) -> Self {
        self.header = name.into().to_ascii_lowercase();
        self
    }

    /// Name of the form field carrying the token (default `csrf_token`)
    pub fn field_name(mut self, name: impl Into<String>) -> Self {
        self.field = name.into();
        self
    }

    /// Accept requests from this origin too, e.g. `https://admin.example.com`
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.allowed_origins.push(origin.into());
        self
    }

    /// Skip all checks for requests where `f` returns true, e.g. API calls
    /// authenticated by a bearer token instead of a cookie
    pub fn exempt<F>(mut self, f: F) -> Self
    where
        F: Fn(&Request) -> bool + Send + Sync + 'static,
    {
        self.exempt = Some(Arc::new(f));
        self
    }

    fn origin_ok(&self, req: &Request) -> bool {
        let header = |name: &str| req.headers.get(name).and_then(|v| v.to_str().ok());
        if let Some(origin) = header("origin") {
            if self.allowed_origins.iter().any(|o| o == origin) {
                return true;
            }
            // `null` (sandboxed frames, some redirects) is never ours
            let authority = origin.split_once("://").map(|(_, rest)| rest);
            return match (authority, header("host")) {
                (Some(a), Some(h)) => a.eq_ignore_ascii_case(h),
                _ => false,
            };
        }
        match header("sec-fetch-site") {
            Some("same-origin") | Some("none") | None => true,
            Some(_) => false,
        }
    }

    fn submitted_token(&self, req: &Request) -> Option<String> {
        if let Some(t) = req
            .headers
            .get(self.header.as_str())
            .and_then(|v| v.to_str().ok())
        {
            return Some(t.to_string());
        }
        let content_type = req
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("application/x-www-form-urlencoded") {
            req.form_data().ok()?.get(&self.field).map(str::to_owned)
        } else if content_type.starts_with("multipart/form-data") {
            req.multipart().ok()?.text(&self.field).map(str::to_owned)
        } else {
            None
        }
    }
}

impl Default for Csrf {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Middleware for Csrf {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let safe = matches!(
            req.method,
            http::Method::GET | http::Method::HEAD | http::Method::OPTIONS | http::Method::TRACE
        );
        if safe || self.exempt.as_ref().is_some_and(|f| f(&req)) {
            return next.run(req).await;
        }

        if !self.origin_ok(&req) {
            return Err(Error::forbidden("Cross-site request blocked"));
        }
        // 500 if the session layer is missing: that is a setup bug, not a client error
        let expected: Option<String> = req.session()?.get("_csrf");
        let submitted = self.submitted_token(&req);
        match (expected, submitted) {
            (Some(expected), Some(given))
                if constant_time_eq(expected.as_bytes(), given.as_bytes()) =>
            {
                next.run(req).await
            }
            _ => Err(Error::forbidden("CSRF token missing or invalid")),
        }
    }
}
