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

    /// Parse a path parameter into any `FromStr` type, e.g. `params.parse::<u64>("id")?`
    ///
    /// Answers `400` naming the parameter when it does not parse.
    pub fn parse<T: std::str::FromStr>(&self, key: &str) -> Result<T> {
        self.get(key)?
            .parse()
            .map_err(|_| Error::bad_request(format!("Invalid path parameter '{}'", key)))
    }
}

impl Default for Params {
    fn default() -> Self {
        Self::new()
    }
}

/// Query parameters from URL (also used for `application/x-www-form-urlencoded` bodies)
#[derive(Debug, Clone)]
pub struct Query {
    inner: HashMap<String, String>,
    raw: String,
}

impl Query {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
            raw: String::new(),
        }
    }

    /// Parse `a=1&b=two`. Keys and values are percent-decoded and `+` is a
    /// space. Pairs without `=` are kept with an empty value; if a key repeats,
    /// the last value wins.
    pub fn parse_query_string(query: &str) -> Self {
        let mut q = Query {
            inner: HashMap::new(),
            raw: query.to_string(),
        };
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            q.inner
                .insert(decode_component(key), decode_component(value));
        }
        q
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(|s| s.as_str())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.inner.contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Deserialize into a struct; numbers, booleans and `Option` are converted
    /// from their string form.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        serde_urlencoded::from_str(&self.raw)
            .map_err(|e| Error::bad_request(format!("Invalid query params: {}", e)))
    }
}

fn decode_component(s: &str) -> String {
    let s = s.replace('+', " ");
    urlencoding::decode(&s).map(|c| c.into_owned()).unwrap_or(s)
}

impl Default for Query {
    fn default() -> Self {
        Self::new()
    }
}

/// Type-keyed storage for shared application state
#[derive(Default)]
pub(crate) struct StateMap {
    values: HashMap<std::any::TypeId, Box<dyn std::any::Any + Send + Sync>>,
}

impl StateMap {
    pub(crate) fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.values
            .insert(std::any::TypeId::of::<T>(), Box::new(value));
    }

    pub(crate) fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.values
            .get(&std::any::TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>())
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
    /// Typed per-request data; middleware can attach values for handlers
    /// (authenticated user, request id, ...).
    pub extensions: http::Extensions,
    /// Address of the connected peer (the proxy when behind one)
    pub remote_addr: Option<std::net::SocketAddr>,
    pub(crate) state: Option<std::sync::Arc<StateMap>>,
    pub(crate) shutdown: Option<crate::shutdown::Shutdown>,
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
            extensions: http::Extensions::new(),
            remote_addr: None,
            state: None,
            shutdown: None,
            cookies: OnceCell::new(),
        }
    }

    /// Shared application state registered with `App::state`
    ///
    /// Answers `500` when no value of type `T` was registered: that is a
    /// programming error, not something the client can fix.
    pub fn state<T: Send + Sync + 'static>(&self) -> Result<&T> {
        self.try_state().ok_or_else(|| {
            Error::internal(format!(
                "State of type {} is not registered",
                std::any::type_name::<T>()
            ))
        })
    }

    /// Signal that fires when the server begins shutting down
    ///
    /// Long-lived handlers (SSE streams, polling loops) select on it to end
    /// promptly: `tokio::select! { _ = req.shutdown_signal().wait() => break, ... }`.
    pub fn shutdown_signal(&self) -> crate::shutdown::Shutdown {
        self.shutdown
            .clone()
            .unwrap_or_else(crate::shutdown::Shutdown::never)
    }

    /// Like [`Request::state`], `None` when the type is not registered
    pub fn try_state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.state.as_ref()?.get::<T>()
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

    /// Parse an `application/x-www-form-urlencoded` body into a struct
    ///
    /// Answers `415` for other content types and `400` for values that do not
    /// fit the target type.
    pub fn form<T: DeserializeOwned>(&self) -> Result<T> {
        self.require_form_content_type()?;
        serde_urlencoded::from_bytes(&self.body)
            .map_err(|e| Error::bad_request(format!("Invalid form data: {}", e)))
    }

    /// The same body as decoded key/value access (`+` and `%XX` decoded)
    pub fn form_data(&self) -> Result<Query> {
        self.require_form_content_type()?;
        let text = std::str::from_utf8(&self.body)
            .map_err(|_| Error::bad_request("Form data is not valid UTF-8"))?;
        Ok(Query::parse_query_string(text))
    }

    fn require_form_content_type(&self) -> Result<()> {
        let is_form = self
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
            .unwrap_or(false);
        if is_form {
            Ok(())
        } else {
            Err(Error::custom(
                415,
                "Expected content-type application/x-www-form-urlencoded",
            ))
        }
    }

    /// Parse JSON body
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body)
            .map_err(|e| Error::bad_request(format!("Invalid JSON: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form_request(content_type: Option<&str>, body: &'static str) -> Request {
        let mut headers = http::HeaderMap::new();
        if let Some(ct) = content_type {
            headers.insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
        }
        Request::new(
            http::Method::POST,
            "/".parse().unwrap(),
            headers,
            bytes::Bytes::from_static(body.as_bytes()),
        )
    }

    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Signup {
        name: String,
        age: u8,
        newsletter: Option<bool>,
    }

    #[test]
    fn params_parse_typed_values() {
        let mut p = Params::new();
        p.insert("id".into(), "42".into());
        p.insert("name".into(), "x".into());
        assert_eq!(p.parse::<u64>("id").unwrap(), 42);
        assert_eq!(p.parse::<String>("name").unwrap(), "x");
        let bad = p.parse::<u64>("name").unwrap_err();
        assert_eq!(bad.status_code(), 400);
        assert_eq!(bad.message(), "Invalid path parameter 'name'");
        assert_eq!(p.parse::<u64>("missing").unwrap_err().status_code(), 400);
        assert!(p.parse::<u8>("id").is_ok());
        p.insert("big".into(), "300".into());
        assert!(p.parse::<u8>("big").is_err());
    }

    #[test]
    fn query_decodes_keys_values_and_plus() {
        let q = Query::parse_query_string("q=rust+web&na%20me=a%26b&flag&&x=1");
        assert_eq!(q.get("q"), Some("rust web"));
        assert_eq!(q.get("na me"), Some("a&b"));
        assert_eq!(q.get("flag"), Some(""));
        assert!(q.contains("x") && !q.is_empty());
    }

    #[test]
    fn query_parse_converts_types() {
        let q = Query::parse_query_string("name=Ann&age=31&newsletter=true");
        assert_eq!(
            q.parse::<Signup>().unwrap(),
            Signup {
                name: "Ann".into(),
                age: 31,
                newsletter: Some(true)
            }
        );
        let bad = Query::parse_query_string("name=Ann&age=old");
        assert_eq!(bad.parse::<Signup>().unwrap_err().status_code(), 400);
    }

    #[test]
    fn form_typed_and_untyped() {
        let req = form_request(
            Some("application/x-www-form-urlencoded; charset=UTF-8"),
            "name=Ann+Lee&age=31",
        );
        let signup: Signup = req.form().unwrap();
        assert_eq!(
            signup,
            Signup {
                name: "Ann Lee".into(),
                age: 31,
                newsletter: None
            }
        );
        assert_eq!(req.form_data().unwrap().get("name"), Some("Ann Lee"));
    }

    #[test]
    fn form_rejects_wrong_content_type_and_bad_values() {
        let json = form_request(Some("application/json"), "name=Ann&age=1");
        assert_eq!(json.form::<Signup>().unwrap_err().status_code(), 415);
        let none = form_request(None, "name=Ann&age=1");
        assert_eq!(none.form_data().unwrap_err().status_code(), 415);

        let bad = form_request(
            Some("application/x-www-form-urlencoded"),
            "name=Ann&age=300",
        );
        assert_eq!(bad.form::<Signup>().unwrap_err().status_code(), 400);
    }
}
