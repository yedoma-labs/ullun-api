//! Cross-origin resource sharing (CORS)
//!
//! Browsers only let a page read a cross-origin response when the server opts
//! in. [`Cors`] implements that opt-in with the pitfalls handled:
//!
//! * the request's `Origin` is checked against the allow list and echoed back
//!   (never "the first configured origin"), with `Vary: Origin` so caches do not
//!   serve one origin's answer to another
//! * credentials are never combined with a wildcard: with
//!   [`Cors::allow_any_origin`] the `Access-Control-Allow-Credentials` header is
//!   simply not sent, whatever else is configured
//! * a preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) is
//!   answered by the middleware itself, even for paths without an `OPTIONS`
//!   route; preflights from unknown origins or for unlisted methods get `403`
//! * error responses carry the CORS headers too, so the browser shows the real
//!   status instead of an opaque network error
//!
//! ```no_run
//! use std::time::Duration;
//! use ullun::cors::Cors;
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(
//!         Cors::new()
//!             .allow_origin("https://app.example.com")
//!             .allow_credentials()
//!             .expose_headers(["x-request-id"])
//!             .max_age(Duration::from_secs(3600)),
//!     )
//!     .get("/", |_req: Request| async { Ok(Response::text("ok")) })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```

use crate::error::Result;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use http::header::{HeaderValue, VARY};
use http::{Method, StatusCode};
use std::sync::Arc;
use std::time::Duration;

type OriginFn = dyn Fn(&str) -> bool + Send + Sync;

enum Origins {
    List(Vec<String>),
    Any,
    Predicate(Arc<OriginFn>),
}

/// CORS middleware, see the [module documentation](self)
pub struct Cors {
    origins: Origins,
    methods: Vec<Method>,
    /// `None` mirrors `Access-Control-Request-Headers`
    headers: Option<Vec<String>>,
    expose: Vec<String>,
    credentials: bool,
    max_age: Duration,
    private_network: bool,
}

impl Default for Cors {
    fn default() -> Self {
        Self::new()
    }
}

impl Cors {
    /// No origin allowed yet; add some with [`allow_origin`](Cors::allow_origin),
    /// [`allow_origins`](Cors::allow_origins), [`allow_origin_fn`](Cors::allow_origin_fn)
    /// or [`allow_any_origin`](Cors::allow_any_origin)
    pub fn new() -> Self {
        Self {
            origins: Origins::List(Vec::new()),
            methods: vec![
                Method::GET,
                Method::HEAD,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ],
            headers: None,
            expose: Vec::new(),
            credentials: false,
            max_age: Duration::from_secs(600),
            private_network: false,
        }
    }

    /// Allow one origin, written exactly as browsers send it
    /// (`https://app.example.com`, no trailing slash)
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        let origin = origin.into();
        match &mut self.origins {
            Origins::List(list) => list.push(origin),
            _ => self.origins = Origins::List(vec![origin]),
        }
        self
    }

    pub fn allow_origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for origin in origins {
            self = self.allow_origin(origin);
        }
        self
    }

    /// Allow every origin (`Access-Control-Allow-Origin: *`). Credentials are
    /// never allowed in this mode.
    pub fn allow_any_origin(mut self) -> Self {
        self.origins = Origins::Any;
        self
    }

    /// Decide per origin, e.g. all subdomains of your own domain
    pub fn allow_origin_fn<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.origins = Origins::Predicate(Arc::new(f));
        self
    }

    /// Methods a cross-origin page may use (default: GET, HEAD, POST, PUT, PATCH, DELETE)
    pub fn allow_methods<I>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = Method>,
    {
        self.methods = methods.into_iter().collect();
        self
    }

    /// Request headers a cross-origin page may send. Without this call the
    /// headers named in the preflight are allowed.
    pub fn allow_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers = Some(headers.into_iter().map(Into::into).collect());
        self
    }

    /// Response headers a cross-origin page may read besides the safelisted ones
    pub fn expose_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.expose = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Let the page send cookies and `Authorization` (`Access-Control-Allow-Credentials`).
    /// Ignored together with [`allow_any_origin`](Cors::allow_any_origin).
    pub fn allow_credentials(mut self) -> Self {
        self.credentials = true;
        self
    }

    /// How long browsers may cache a preflight answer (default 10 minutes)
    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// Answer Private Network Access preflights, which public pages send before
    /// reaching into a private network (`Access-Control-Allow-Private-Network`)
    pub fn allow_private_network(mut self) -> Self {
        self.private_network = true;
        self
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        match &self.origins {
            Origins::Any => true,
            Origins::List(list) => list.iter().any(|o| o == origin),
            Origins::Predicate(f) => f(origin),
        }
    }

    fn any(&self) -> bool {
        matches!(self.origins, Origins::Any)
    }

    /// `Access-Control-Allow-Origin` and friends for an allowed origin
    fn decorate(&self, mut res: Response, origin: &str) -> Response {
        if self.any() {
            res = res.with_header("access-control-allow-origin", "*");
        } else {
            res = res.with_header("access-control-allow-origin", origin);
            add_vary(&mut res, "origin");
            if self.credentials {
                res = res.with_header("access-control-allow-credentials", "true");
            }
        }
        if !self.expose.is_empty() {
            res = res.with_header("access-control-expose-headers", self.expose.join(", "));
        }
        res
    }

    fn preflight(&self, req: &Request, origin: &str) -> Response {
        let requested = req
            .headers
            .get("access-control-request-method")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let method_ok = self.methods.iter().any(|m| m.as_str() == requested);
        if !self.origin_allowed(origin) || !method_ok {
            let mut res = Response::new(StatusCode::FORBIDDEN);
            add_vary(&mut res, "origin");
            return res;
        }

        let mut res = self.decorate(Response::new(StatusCode::NO_CONTENT), origin);
        let methods: Vec<&str> = self.methods.iter().map(Method::as_str).collect();
        res = res
            .with_header("access-control-allow-methods", methods.join(", "))
            .with_header("access-control-max-age", self.max_age.as_secs().to_string());
        match &self.headers {
            Some(list) if !list.is_empty() => {
                res = res.with_header("access-control-allow-headers", list.join(", "));
            }
            Some(_) => {}
            None => {
                if let Some(asked) = req
                    .headers
                    .get("access-control-request-headers")
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| !v.trim().is_empty())
                {
                    res = res.with_header("access-control-allow-headers", asked);
                    add_vary(&mut res, "access-control-request-headers");
                }
            }
        }
        if self.private_network
            && req
                .headers
                .get("access-control-request-private-network")
                .is_some_and(|v| v == "true")
        {
            res = res.with_header("access-control-allow-private-network", "true");
        }
        res
    }
}

/// Add a token to `Vary` without dropping the ones already there
pub(crate) fn add_vary(res: &mut Response, token: &str) {
    let existing: Vec<String> = res
        .headers
        .get_all(VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if existing
        .iter()
        .any(|t| t == "*" || t.eq_ignore_ascii_case(token))
    {
        return;
    }
    let mut all = existing;
    all.push(token.to_string());
    if let Ok(value) = HeaderValue::from_str(&all.join(", ")) {
        res.headers.insert(VARY, value);
    }
}

#[async_trait]
impl Middleware for Cors {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let origin = req
            .headers
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(origin) = origin else {
            // same-origin or non-browser request: nothing to negotiate, but a
            // cache must still keep the variants apart
            let mut res = into_response(next.run(req).await);
            if !self.any() {
                add_vary(&mut res, "origin");
            }
            return Ok(res);
        };

        let is_preflight = req.method == Method::OPTIONS
            && req.headers.contains_key("access-control-request-method");
        if is_preflight {
            return Ok(self.preflight(&req, &origin));
        }

        let mut res = into_response(next.run(req).await);
        if self.origin_allowed(&origin) {
            res = self.decorate(res, &origin);
        } else if !self.any() {
            add_vary(&mut res, "origin");
        }
        Ok(res)
    }
}

fn into_response(result: Result<Response>) -> Response {
    result.unwrap_or_else(Response::from_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vary_is_merged_not_replaced() {
        let mut res = Response::ok().with_header("vary", "accept-encoding");
        add_vary(&mut res, "origin");
        assert_eq!(res.headers[VARY], "accept-encoding, origin");
        add_vary(&mut res, "Origin");
        assert_eq!(res.headers[VARY], "accept-encoding, origin");
    }

    #[test]
    fn vary_star_is_left_alone() {
        let mut res = Response::ok().with_header("vary", "*");
        add_vary(&mut res, "origin");
        assert_eq!(res.headers[VARY], "*");
    }

    #[test]
    fn origin_matching_is_exact() {
        let cors = Cors::new().allow_origin("https://a.example");
        assert!(cors.origin_allowed("https://a.example"));
        assert!(!cors.origin_allowed("https://a.example.evil.com"));
        assert!(!cors.origin_allowed("http://a.example"));
        assert!(!Cors::new().origin_allowed("null"));
    }
}
