//! Test an [`App`] in-process, without sockets
//!
//! [`TestClient`] sends requests straight through routing, middleware and
//! handlers, exactly as a running server would, and returns the response. No
//! port is opened, so tests are fast and cannot collide.
//!
//! ```
//! use ullun::prelude::*;
//! use ullun::testing::TestClient;
//!
//! # tokio_test_block(async {
//! let app = App::new().post("/echo", |req: Request| async move {
//!     let name: String = req.json::<serde_json::Value>()?["name"]
//!         .as_str()
//!         .unwrap_or("nobody")
//!         .to_string();
//!     Ok(Response::json(serde_json::json!({ "hello": name })))
//! });
//! let client = TestClient::new(app);
//!
//! let res = client.post("/echo").json(&serde_json::json!({ "name": "Ada" })).send().await;
//! assert_eq!(res.status(), 200);
//! assert_eq!(res.json::<serde_json::Value>()["hello"], "Ada");
//! # });
//! # fn tokio_test_block<F: std::future::Future>(f: F) {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f);
//! # }
//! ```
//!
//! [`TestClient::with_cookies`] keeps a cookie jar between calls, which makes
//! login flows and sessions testable. Background tasks registered with
//! `App::background` are not started, and WebSocket upgrades need a real
//! server (`App::serve` on port 0).

use crate::app::{App, InProcess};
use crate::request::Request;
use bytes::Bytes;
use http_body_util::BodyExt;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// Calls an app in-process, see the [module documentation](self)
#[derive(Clone)]
pub struct TestClient {
    server: Arc<InProcess>,
    jar: Option<Arc<Mutex<BTreeMap<String, String>>>>,
}

impl TestClient {
    pub fn new(app: App) -> Self {
        Self {
            server: Arc::new(app.into_in_process()),
            jar: None,
        }
    }

    /// Remember cookies set by responses and send them on later requests
    pub fn with_cookies(mut self) -> Self {
        self.jar = Some(Arc::default());
        self
    }

    /// Value of a cookie in the jar
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.jar
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// Forget all cookies
    pub fn clear_cookies(&self) {
        if let Some(jar) = &self.jar {
            jar.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    pub fn request(&self, method: &str, path: &str) -> TestRequest {
        TestRequest {
            client: self.clone(),
            method: method.to_string(),
            path: path.to_string(),
            headers: Vec::new(),
            body: Bytes::new(),
            peer: ([127, 0, 0, 1], 50_000).into(),
            secure: false,
        }
    }

    pub fn get(&self, path: &str) -> TestRequest {
        self.request("GET", path)
    }
    pub fn post(&self, path: &str) -> TestRequest {
        self.request("POST", path)
    }
    pub fn put(&self, path: &str) -> TestRequest {
        self.request("PUT", path)
    }
    pub fn patch(&self, path: &str) -> TestRequest {
        self.request("PATCH", path)
    }
    pub fn delete(&self, path: &str) -> TestRequest {
        self.request("DELETE", path)
    }
    pub fn head(&self, path: &str) -> TestRequest {
        self.request("HEAD", path)
    }
    pub fn options(&self, path: &str) -> TestRequest {
        self.request("OPTIONS", path)
    }

    fn remember(&self, headers: &http::HeaderMap) {
        let Some(jar) = &self.jar else { return };
        let mut jar = jar.lock().unwrap_or_else(|e| e.into_inner());
        for value in headers.get_all(http::header::SET_COOKIE) {
            let Ok(text) = value.to_str() else { continue };
            let mut parts = text.split(';').map(str::trim);
            let Some((name, val)) = parts.next().and_then(|nv| nv.split_once('=')) else {
                continue;
            };
            let expired = parts.any(|attr| {
                attr.eq_ignore_ascii_case("max-age=0")
                    || attr.to_ascii_lowercase().starts_with("max-age=-")
            });
            if expired || val.is_empty() {
                jar.remove(name);
            } else {
                jar.insert(name.to_string(), val.to_string());
            }
        }
    }
}

/// A request under construction
pub struct TestRequest {
    client: TestClient,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Bytes,
    peer: SocketAddr,
    secure: bool,
}

impl TestRequest {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    /// Send a cookie in addition to those from the jar
    pub fn cookie(self, name: &str, value: &str) -> Self {
        self.header("cookie", &format!("{name}={value}"))
    }

    pub fn json<T: Serialize>(mut self, value: &T) -> Self {
        self.headers
            .push(("content-type".into(), "application/json".into()));
        self.body = Bytes::from(serde_json::to_vec(value).unwrap_or_default());
        self
    }

    /// `application/x-www-form-urlencoded` body
    pub fn form<T: Serialize>(mut self, value: &T) -> Self {
        self.headers.push((
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        self.body = Bytes::from(serde_urlencoded::to_string(value).unwrap_or_default());
        self
    }

    pub fn text(mut self, body: &str) -> Self {
        self.headers
            .push(("content-type".into(), "text/plain; charset=utf-8".into()));
        self.body = Bytes::from(body.to_string());
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// Pretend the request comes from this peer address
    pub fn remote_addr(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.peer = addr.into();
        self
    }

    /// Pretend the request arrived over TLS (`Request::is_secure`)
    pub fn https(mut self) -> Self {
        self.secure = true;
        self
    }

    pub async fn send(self) -> TestResponse {
        let uri: http::Uri = self
            .path
            .parse()
            .expect("test request path is not a valid URI");
        let method = http::Method::from_bytes(self.method.as_bytes()).expect("invalid method");
        let mut headers = http::HeaderMap::new();
        for (name, value) in &self.headers {
            let (Ok(n), Ok(v)) = (
                http::HeaderName::from_bytes(name.as_bytes()),
                http::HeaderValue::from_str(value),
            ) else {
                panic!("invalid test header {name}: {value}");
            };
            headers.append(n, v);
        }
        // "localhost" unless the test chose a Host itself
        if !headers.contains_key(http::header::HOST) {
            headers.insert(
                http::header::HOST,
                http::HeaderValue::from_static("localhost"),
            );
        }
        if let Some(jar) = &self.client.jar {
            let jar = jar.lock().unwrap_or_else(|e| e.into_inner());
            if !jar.is_empty() {
                let joined = jar
                    .iter()
                    .map(|(n, v)| format!("{n}={v}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                headers.append(
                    http::header::COOKIE,
                    http::HeaderValue::from_str(&joined).expect("cookie jar value"),
                );
            }
        }
        if !self.body.is_empty() {
            headers.insert(http::header::CONTENT_LENGTH, self.body.len().into());
        }

        let request = Request::new(method, uri, headers, self.body);
        let res = self
            .client
            .server
            .call(request, self.peer, self.secure)
            .await;
        let (parts, body) = res.into_parts();
        // a stream that never ends must not hang the test
        let body = tokio::time::timeout(std::time::Duration::from_secs(10), body.collect())
            .await
            .expect("response body did not finish within 10 seconds")
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        self.client.remember(&parts.headers);
        TestResponse {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body,
        }
    }
}

/// What the app answered
#[derive(Debug, Clone)]
pub struct TestResponse {
    status: u16,
    headers: http::HeaderMap,
    body: Bytes,
}

impl TestResponse {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }

    /// Header value, empty when absent or not text
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    pub fn bytes(&self) -> &Bytes {
        &self.body
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Body parsed as JSON; panics with the body in the message when it is not
    pub fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("response is not the expected JSON ({e}): {}", self.text()))
    }
}
