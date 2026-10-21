//! RFC 9457 problem details for error responses
//!
//! By default errors are `{"error": "...", "status": 404}`. Add
//! [`ProblemDetails`] to answer in the standard format instead, which API
//! clients and gateways understand without knowing your conventions:
//!
//! ```json
//! {
//!   "type": "https://api.example.com/problems/not-found",
//!   "title": "Not Found",
//!   "status": 404,
//!   "detail": "Order 17 does not exist",
//!   "instance": "/orders/17",
//!   "errors": { "quantity": ["must be 1-50"] }
//! }
//! ```
//!
//! sent as `application/problem+json`. Validation failures (422) list their
//! fields under `errors`.
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::problem::ProblemDetails;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(ProblemDetails::new().type_base("https://api.example.com/problems/"))
//!     .get("/orders/{id}", |_req: Request| async {
//!         Err::<Response, _>(Error::not_found("Order does not exist"))
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Only the framework's own error bodies are converted (a JSON object with
//! exactly `error` and `status`, plus `fields` for validation); responses your
//! handlers build themselves are left alone. Without
//! [`type_base`](ProblemDetails::type_base) the `type` is `about:blank`, as the RFC
//! allows for plain HTTP status problems.

use crate::error::Result;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;

/// Middleware turning error responses into `application/problem+json`
#[derive(Clone, Debug, Default)]
pub struct ProblemDetails {
    type_base: Option<String>,
    include_instance: bool,
}

impl ProblemDetails {
    pub fn new() -> Self {
        Self {
            type_base: None,
            include_instance: true,
        }
    }

    /// Prefix for the `type` URI: `{base}not-found`, `{base}validation-failed`, ...
    pub fn type_base(mut self, base: &str) -> Self {
        self.type_base = Some(base.to_string());
        self
    }

    /// Leave out `instance` (the request path)
    pub fn without_instance(mut self) -> Self {
        self.include_instance = false;
        self
    }

    fn convert(&self, res: Response, path: &str) -> Response {
        if !res.status.is_client_error() && !res.status.is_server_error() {
            return res;
        }
        let is_json = res
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        if !is_json || res.is_streaming() {
            return res;
        }
        let Ok(serde_json::Value::Object(old)) = serde_json::from_slice(&res.body) else {
            return res;
        };
        let known = old
            .keys()
            .all(|k| matches!(k.as_str(), "error" | "status" | "fields"));
        let (Some(detail), Some(status)) = (
            old.get("error").and_then(|v| v.as_str()),
            old.get("status").and_then(|v| v.as_u64()),
        ) else {
            return res;
        };
        if !known || status != u64::from(res.status.as_u16()) {
            return res;
        }

        let reason = res.status.canonical_reason().unwrap_or("Error");
        let slug = if old.contains_key("fields") {
            "validation-failed".to_string()
        } else {
            reason.to_ascii_lowercase().replace(' ', "-")
        };
        let mut problem = serde_json::json!({
            "type": match &self.type_base {
                Some(base) => format!("{base}{slug}"),
                None => "about:blank".to_string(),
            },
            "title": reason,
            "status": status,
            "detail": detail,
        });
        if self.include_instance {
            problem["instance"] = path.into();
        }
        if let Some(fields) = old.get("fields") {
            problem["errors"] = fields.clone();
        }
        let mut out = res;
        out.body = serde_json::to_vec(&problem).unwrap_or_default().into();
        out.headers.remove(http::header::CONTENT_LENGTH);
        out.with_header("content-type", "application/problem+json")
    }
}

#[async_trait]
impl Middleware for ProblemDetails {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let path = req.uri.path().to_string();
        let res = next.run(req).await.unwrap_or_else(Response::from_error);
        Ok(self.convert(res, &path))
    }
}
