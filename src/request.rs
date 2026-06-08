//! Request types and extractors

use crate::cookies::Cookies;
use crate::error::{Error, Result};
use serde::de::DeserializeOwned;
use std::cell::OnceCell;
use std::collections::HashMap;

/// Path parameters extracted from route
#[derive(Debug, Clone)]
pub struct Params {
    inner: HashMap<String, String>,
}

impl Params {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: String, value: String) {
        self.inner.insert(key, value);
    }

    pub fn get(&self, key: &str) -> Result<&str> {
        self.inner
            .get(key)
            .map(|s| s.as_str())
            .ok_or_else(|| Error::bad_request(format!("Missing param: {}", key)))
    }

    pub fn get_opt(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(|s| s.as_str())
    }
}

impl Default for Params {
    fn default() -> Self {
        Self::new()
    }
}

/// Query parameters from URL
#[derive(Debug, Clone)]
pub struct Query {
    inner: HashMap<String, String>,
}

impl Query {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    pub fn parse_query_string(query: &str) -> Self {
        let mut q = Query::new();
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                q.inner.insert(
                    key.to_string(),
                    urlencoding::decode(value).unwrap_or_default().to_string(),
                );
            }
        }
        q
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(|s| s.as_str())
    }

    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        let value = serde_json::to_value(&self.inner)
            .map_err(|e| Error::internal(format!("Failed to serialize query params: {}", e)))?;
        serde_json::from_value(value)
            .map_err(|e| Error::bad_request(format!("Invalid query params: {}", e)))
    }
}

impl Default for Query {
    fn default() -> Self {
        Self::new()
    }
}

/// HTTP Request wrapper
pub struct Request {
    pub method: http::Method,
    pub uri: http::Uri,
    pub headers: http::HeaderMap,
    pub body: bytes::Bytes,
    pub params: Params,
    pub query: Query,
    cookies: OnceCell<Cookies>,
}

impl Request {
    pub fn new(
        method: http::Method,
        uri: http::Uri,
        headers: http::HeaderMap,
        body: bytes::Bytes,
    ) -> Self {
        let query = uri
            .query()
            .map(Query::parse_query_string)
            .unwrap_or_default();

        Self {
            method,
            uri,
            headers,
            body,
            params: Params::new(),
            query,
            cookies: OnceCell::new(),
        }
    }

    /// Get cookies from the request
    pub fn cookies(&self) -> &Cookies {
        self.cookies.get_or_init(|| {
            if let Some(cookie_header) = self.headers.get("cookie") {
                if let Ok(cookie_str) = cookie_header.to_str() {
                    Cookies::parse(cookie_str)
                } else {
                    Cookies::new()
                }
            } else {
                Cookies::new()
            }
        })
    }

    /// Parse JSON body
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body)
            .map_err(|e| Error::bad_request(format!("Invalid JSON: {}", e)))
    }
}
