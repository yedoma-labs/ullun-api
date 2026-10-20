//! Security response headers, including a Content-Security-Policy builder
//!
//! [`SecurityHeaders`] adds the headers every response should carry. A header
//! the handler already set is left alone, so one route can loosen the policy.
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::security_headers::{Csp, SecurityHeaders};
//!
//! # async fn run() {
//! App::new()
//!     .middleware(SecurityHeaders::new().csp(Csp::web().script_src(["'self'", "'nonce-{nonce}'"])))
//!     .get("/", |req: Request| async move {
//!         // the nonce is fresh for every request
//!         let nonce = req.csp_nonce().unwrap_or_default().to_string();
//!         Ok(Response::html(format!("<script nonce=\"{nonce}\">console.log(1)</script>")))
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Defaults of [`SecurityHeaders::new`]: `X-Content-Type-Options: nosniff`,
//! `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`,
//! `Cross-Origin-Opener-Policy: same-origin`,
//! `Cross-Origin-Resource-Policy: same-origin` and a `Permissions-Policy` that
//! switches off camera, microphone, geolocation and payment. HSTS is a separate
//! middleware ([`crate::secure::Hsts`]) because it only belongs on HTTPS.

use crate::error::Result;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;

/// The per-request nonce generated for a policy that uses the `{nonce}` placeholder
#[derive(Clone, Debug)]
pub(crate) struct CspNonce(pub(crate) String);

/// A Content-Security-Policy under construction
///
/// A value of `'nonce-{nonce}'` in any directive is replaced with a fresh random
/// nonce per request, readable through [`Request::csp_nonce`] for inline
/// `<script nonce="...">` tags.
#[derive(Clone, Debug, Default)]
pub struct Csp {
    directives: Vec<(String, Vec<String>)>,
    report_only: bool,
}

impl Csp {
    /// Empty policy
    pub fn new() -> Self {
        Self::default()
    }

    /// For JSON APIs: nothing may load or embed anything
    pub fn api() -> Self {
        Self::new()
            .directive("default-src", ["'none'"])
            .directive("frame-ancestors", ["'none'"])
    }

    /// For server-rendered pages: same-origin resources only, no plugins, no
    /// framing, forms post to the same origin
    pub fn web() -> Self {
        Self::new()
            .directive("default-src", ["'self'"])
            .directive("base-uri", ["'self'"])
            .directive("form-action", ["'self'"])
            .directive("frame-ancestors", ["'none'"])
            .directive("object-src", ["'none'"])
    }

    /// Set (replace) a directive
    pub fn directive<I, S>(mut self, name: &str, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let values: Vec<String> = values.into_iter().map(Into::into).collect();
        match self.directives.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => *v = values,
            None => self.directives.push((name.to_string(), values)),
        }
        self
    }

    pub fn script_src<I, S>(self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.directive("script-src", values)
    }

    pub fn style_src<I, S>(self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.directive("style-src", values)
    }

    pub fn img_src<I, S>(self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.directive("img-src", values)
    }

    pub fn connect_src<I, S>(self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.directive("connect-src", values)
    }

    /// Where browsers send violation reports (`report-uri`)
    pub fn report_uri(self, uri: &str) -> Self {
        self.directive("report-uri", [uri])
    }

    /// Deliver as `Content-Security-Policy-Report-Only`: violations are reported,
    /// nothing is blocked. Useful to roll a policy out.
    pub fn report_only(mut self) -> Self {
        self.report_only = true;
        self
    }

    fn uses_nonce(&self) -> bool {
        self.directives
            .iter()
            .any(|(_, v)| v.iter().any(|s| s.contains("{nonce}")))
    }

    fn render(&self, nonce: &str) -> String {
        self.directives
            .iter()
            .map(|(name, values)| {
                if values.is_empty() {
                    name.clone()
                } else {
                    format!("{} {}", name, values.join(" ").replace("{nonce}", nonce))
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Value of `X-Frame-Options`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOptions {
    Deny,
    SameOrigin,
}

/// Middleware adding security headers, see the [module documentation](self)
#[derive(Clone, Debug)]
pub struct SecurityHeaders {
    csp: Option<Csp>,
    frame_options: Option<FrameOptions>,
    headers: Vec<(String, String)>,
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        Self::new()
    }
}

impl SecurityHeaders {
    pub fn new() -> Self {
        Self {
            csp: None,
            frame_options: Some(FrameOptions::Deny),
            headers: vec![
                ("x-content-type-options".into(), "nosniff".into()),
                ("referrer-policy".into(), "no-referrer".into()),
                ("cross-origin-opener-policy".into(), "same-origin".into()),
                ("cross-origin-resource-policy".into(), "same-origin".into()),
                (
                    "permissions-policy".into(),
                    "camera=(), microphone=(), geolocation=(), payment=()".into(),
                ),
            ],
        }
    }

    /// Send a Content-Security-Policy (none by default: a good policy depends on the app)
    pub fn csp(mut self, csp: Csp) -> Self {
        self.csp = Some(csp);
        self
    }

    /// `X-Frame-Options` (default `DENY`); `None` omits it, e.g. when the CSP's
    /// `frame-ancestors` should be the only rule
    pub fn frame_options(mut self, options: impl Into<Option<FrameOptions>>) -> Self {
        self.frame_options = options.into();
        self
    }

    /// Set or replace any other header
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name = name.to_ascii_lowercase();
        match self.headers.iter_mut().find(|(n, _)| *n == name) {
            Some((_, v)) => *v = value.to_string(),
            None => self.headers.push((name, value.to_string())),
        }
        self
    }

    /// Drop one of the default headers
    pub fn without(mut self, name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        self.headers.retain(|(n, _)| *n != name);
        self
    }
}

#[async_trait]
impl Middleware for SecurityHeaders {
    async fn handle(&self, mut req: Request, next: Next) -> Result<Response> {
        let mut nonce = String::new();
        if self.csp.as_ref().is_some_and(Csp::uses_nonce) {
            nonce = crate::security::random_token(16)?;
            req.extensions.insert(CspNonce(nonce.clone()));
        }
        let mut res = next.run(req).await.unwrap_or_else(Response::from_error);

        let put = |res: &mut Response, name: &str, value: String| {
            if !res.headers.contains_key(name) {
                if let (Ok(n), Ok(v)) = (
                    http::HeaderName::from_bytes(name.as_bytes()),
                    http::HeaderValue::from_str(&value),
                ) {
                    res.headers.insert(n, v);
                }
            }
        };
        for (name, value) in &self.headers {
            put(&mut res, name, value.clone());
        }
        match self.frame_options {
            Some(FrameOptions::Deny) => put(&mut res, "x-frame-options", "DENY".into()),
            Some(FrameOptions::SameOrigin) => put(&mut res, "x-frame-options", "SAMEORIGIN".into()),
            None => {}
        }
        if let Some(csp) = &self.csp {
            let name = if csp.report_only {
                "content-security-policy-report-only"
            } else {
                "content-security-policy"
            };
            put(&mut res, name, csp.render(&nonce));
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_directives_and_nonce() {
        let csp = Csp::web().script_src(["'self'", "'nonce-{nonce}'"]);
        assert!(csp.uses_nonce());
        let text = csp.render("abc");
        assert!(text.starts_with("default-src 'self'; "));
        assert!(text.contains("script-src 'self' 'nonce-abc'"));
        assert!(text.contains("frame-ancestors 'none'"));
    }

    #[test]
    fn directive_replaces_instead_of_duplicating() {
        let csp = Csp::api().directive("default-src", ["'self'"]);
        assert_eq!(csp.render(""), "default-src 'self'; frame-ancestors 'none'");
    }

    #[test]
    fn valueless_directive_renders_as_a_flag() {
        let csp = Csp::new().directive("upgrade-insecure-requests", Vec::<String>::new());
        assert_eq!(csp.render(""), "upgrade-insecure-requests");
    }
}
