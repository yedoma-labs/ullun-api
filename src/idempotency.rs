//! Idempotency keys for unsafe requests
//!
//! Networks retry. A client that times out on `POST /payments` cannot know
//! whether the payment happened, and resending may charge twice. With the
//! [`Idempotency`] middleware the client sends an `Idempotency-Key` header
//! (a random string it reuses for retries of the same operation) and the server
//! runs the handler only once:
//!
//! * first request: handler runs, a successful (non-5xx, non-streaming) response is stored
//! * retry with the same key and payload: the stored response is replayed
//!   with `Idempotency-Replayed: true`, the handler does not run again
//! * retry while the first is still running: `409 Conflict` + `Retry-After`
//! * same key, different payload (method, path, query or body): `422`
//! * a `5xx`, a streaming response, a body over [`Idempotency::max_response_bytes`]
//!   or a handler that was cancelled releases the key, so a retry can try again
//!
//! Keys are scoped to the caller: by default to the `Authorization` and `Cookie`
//! headers, so one user cannot replay (and read) another user's response.
//! Change it with [`Idempotency::scope_by`], e.g. to a user id.
//!
//! ```no_run
//! use ullun::idempotency::Idempotency;
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(Idempotency::new())
//!     .post("/payments", |_req: Request| async {
//!         Ok(Response::json(serde_json::json!({ "charged": true })))
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! The default store lives in memory of one process. Implement
//! [`IdempotencyStore`] on top of your database or Redis when several instances
//! serve the same clients.

use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use bytes::Bytes;
use http::Method;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A response kept for replay
#[derive(Clone, Debug)]
pub struct StoredResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Bytes,
}

/// What the store says about a key
#[derive(Debug)]
pub enum Begin {
    /// New key: the caller now owns it and must call `complete` or `abandon`
    Started,
    /// Another request with this key is running
    InFlight,
    /// This key finished before with the same request
    Replay(StoredResponse),
    /// This key was used for a different request
    Mismatch,
    /// The store cannot take more keys right now
    Unavailable,
}

/// Where idempotency keys live
#[async_trait]
pub trait IdempotencyStore: Send + Sync + 'static {
    /// Claim `key` for a request with this `fingerprint`. A claim that is never
    /// completed must lapse after `lock_ttl`.
    async fn begin(&self, key: &str, fingerprint: u64, lock_ttl: Duration) -> Begin;
    /// Keep `response` for `ttl`
    async fn complete(&self, key: &str, response: StoredResponse, ttl: Duration);
    /// Forget a claim so the request can be retried
    async fn abandon(&self, key: &str);
}

enum State {
    Running,
    Done(StoredResponse),
}

struct Entry {
    fingerprint: u64,
    state: State,
    expires: Instant,
}

/// Process-local store, the default
pub struct MemoryStore {
    entries: Mutex<HashMap<String, Entry>>,
    max_entries: usize,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl MemoryStore {
    /// Keep at most `max_entries` keys (default 10 000); when full of live keys new ones are refused (`503`)
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_entries,
        }
    }

    fn begin_at(&self, key: &str, fingerprint: u64, lock_ttl: Duration, now: Instant) -> Begin {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get(key) {
            if entry.expires > now {
                return if entry.fingerprint != fingerprint {
                    Begin::Mismatch
                } else {
                    match &entry.state {
                        State::Running => Begin::InFlight,
                        State::Done(res) => Begin::Replay(res.clone()),
                    }
                };
            }
        }
        if map.len() >= self.max_entries && !map.contains_key(key) {
            map.retain(|_, e| e.expires > now);
            if map.len() >= self.max_entries {
                return Begin::Unavailable;
            }
        }
        map.insert(
            key.to_string(),
            Entry {
                fingerprint,
                state: State::Running,
                expires: now + lock_ttl,
            },
        );
        Begin::Started
    }
}

#[async_trait]
impl IdempotencyStore for MemoryStore {
    async fn begin(&self, key: &str, fingerprint: u64, lock_ttl: Duration) -> Begin {
        self.begin_at(key, fingerprint, lock_ttl, Instant::now())
    }

    async fn complete(&self, key: &str, response: StoredResponse, ttl: Duration) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(key) {
            entry.state = State::Done(response);
            entry.expires = Instant::now() + ttl;
        }
    }

    async fn abandon(&self, key: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(map.get(key).map(|e| &e.state), Some(State::Running)) {
            map.remove(key);
        }
    }
}

type ScopeFn = dyn Fn(&Request) -> String + Send + Sync;

/// Idempotency-key middleware, see the [module documentation](self)
pub struct Idempotency {
    store: Arc<dyn IdempotencyStore>,
    header: String,
    methods: Vec<Method>,
    ttl: Duration,
    lock_ttl: Duration,
    max_response: usize,
    required: bool,
    scope: Arc<ScopeFn>,
    hasher: std::collections::hash_map::RandomState,
}

impl Default for Idempotency {
    fn default() -> Self {
        Self::new()
    }
}

impl Idempotency {
    /// In-memory store, `POST` and `PATCH`, results kept for 24 hours
    pub fn new() -> Self {
        Self::with_store(MemoryStore::default())
    }

    pub fn with_store(store: impl IdempotencyStore) -> Self {
        let hasher = std::collections::hash_map::RandomState::new();
        let scope_hasher = hasher.clone();
        Self {
            store: Arc::new(store),
            header: "idempotency-key".into(),
            methods: vec![Method::POST, Method::PATCH],
            ttl: Duration::from_secs(24 * 3600),
            lock_ttl: Duration::from_secs(60),
            max_response: 256 * 1024,
            required: false,
            scope: Arc::new(move |req| {
                let mut h = scope_hasher.build_hasher();
                for name in ["authorization", "cookie"] {
                    req.headers.get(name).map(|v| v.as_bytes()).hash(&mut h);
                }
                format!("{:016x}", h.finish())
            }),
            hasher,
        }
    }

    /// Methods the key applies to (default `POST` and `PATCH`; `PUT` and `DELETE`
    /// are idempotent by definition)
    pub fn methods<I: IntoIterator<Item = Method>>(mut self, methods: I) -> Self {
        self.methods = methods.into_iter().collect();
        self
    }

    /// How long a finished response can be replayed (default 24 hours)
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// How long a running request blocks retries if it never finishes (default 60 seconds)
    pub fn lock_ttl(mut self, lock_ttl: Duration) -> Self {
        self.lock_ttl = lock_ttl;
        self
    }

    /// Largest response body that is kept for replay (default 256 KiB). Larger
    /// responses are sent normally but not stored, so a retry runs the handler again.
    pub fn max_response_bytes(mut self, max: usize) -> Self {
        self.max_response = max;
        self
    }

    /// Answer `400` when the header is missing instead of running the handler
    /// without protection
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Header carrying the key (default `Idempotency-Key`)
    pub fn header(mut self, name: &str) -> Self {
        self.header = name.to_ascii_lowercase();
        self
    }

    /// Whose keys these are. Keys of different scopes never collide. The default
    /// is the `Authorization` + `Cookie` headers; use a user or tenant id when you have one.
    pub fn scope_by<F>(mut self, f: F) -> Self
    where
        F: Fn(&Request) -> String + Send + Sync + 'static,
    {
        self.scope = Arc::new(f);
        self
    }

    fn fingerprint(&self, req: &Request) -> u64 {
        let mut h = self.hasher.build_hasher();
        req.method.as_str().hash(&mut h);
        req.uri.path().hash(&mut h);
        req.uri.query().hash(&mut h);
        req.body.hash(&mut h);
        h.finish()
    }
}

/// Releases a claim if the request future is dropped before it finishes
struct Claim {
    store: Arc<dyn IdempotencyStore>,
    key: String,
    armed: bool,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.armed {
            let (store, key) = (self.store.clone(), std::mem::take(&mut self.key));
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { store.abandon(&key).await });
            }
        }
    }
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 255 && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

#[async_trait]
impl Middleware for Idempotency {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        if !self.methods.contains(&req.method) {
            return next.run(req).await;
        }
        let key = match req.headers.get(self.header.as_str()) {
            None if self.required => {
                return Ok(Response::from_error(Error::bad_request(format!(
                    "{} header is required",
                    self.header
                ))))
            }
            None => return next.run(req).await,
            Some(v) => match v.to_str().ok().filter(|k| valid_key(k)) {
                Some(k) => k.to_string(),
                None => {
                    return Ok(Response::from_error(Error::bad_request(format!(
                        "{} must be 1-255 visible ASCII characters",
                        self.header
                    ))))
                }
            },
        };

        let store_key = format!("{}|{}", (self.scope)(&req), key);
        let fingerprint = self.fingerprint(&req);
        match self
            .store
            .begin(&store_key, fingerprint, self.lock_ttl)
            .await
        {
            Begin::Started => {}
            Begin::InFlight => {
                return Ok(Response::from_error(Error::custom(
                    409,
                    "A request with this idempotency key is still being processed",
                ))
                .with_header("retry-after", "1"))
            }
            Begin::Mismatch => {
                return Ok(Response::from_error(Error::custom(
                    422,
                    "Idempotency key was already used with a different request",
                )))
            }
            Begin::Unavailable => {
                return Ok(Response::from_error(Error::custom(
                    503,
                    "Idempotency store is full, try again shortly",
                ))
                .with_header("retry-after", "1"))
            }
            Begin::Replay(stored) => return Ok(replay(stored)),
        }

        let mut claim = Claim {
            store: self.store.clone(),
            key: store_key.clone(),
            armed: true,
        };
        let res = next.run(req).await.unwrap_or_else(Response::from_error);
        if res.status.is_server_error() || res.is_streaming() || res.body.len() > self.max_response
        {
            return Ok(res); // claim drops armed: released for a retry
        }
        claim.armed = false;
        let stored = StoredResponse {
            status: res.status.as_u16(),
            headers: res
                .headers
                .iter()
                .filter(|(n, _)| *n != http::header::SET_COOKIE)
                .map(|(n, v)| (n.as_str().to_string(), v.as_bytes().to_vec()))
                .collect(),
            body: res.body.clone(),
        };
        self.store.complete(&store_key, stored, self.ttl).await;
        Ok(res)
    }
}

fn replay(stored: StoredResponse) -> Response {
    let mut res =
        Response::new(http::StatusCode::from_u16(stored.status).unwrap_or(http::StatusCode::OK))
            .with_body(stored.body);
    for (name, value) in stored.headers {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_bytes(&value),
        ) {
            res.headers.append(n, v);
        }
    }
    res.with_header("idempotency-replayed", "true")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done() -> StoredResponse {
        StoredResponse {
            status: 201,
            headers: vec![],
            body: Bytes::from_static(b"x"),
        }
    }

    #[tokio::test]
    async fn lifecycle_in_memory() {
        let s = MemoryStore::default();
        let ttl = Duration::from_secs(10);
        assert!(matches!(s.begin("k", 1, ttl).await, Begin::Started));
        assert!(matches!(s.begin("k", 1, ttl).await, Begin::InFlight));
        assert!(matches!(s.begin("k", 2, ttl).await, Begin::Mismatch));
        s.complete("k", done(), ttl).await;
        assert!(matches!(s.begin("k", 1, ttl).await, Begin::Replay(r) if r.status == 201));
        assert!(matches!(s.begin("k", 2, ttl).await, Begin::Mismatch));
    }

    #[tokio::test]
    async fn abandon_frees_running_but_not_finished_keys() {
        let s = MemoryStore::default();
        let ttl = Duration::from_secs(10);
        s.begin("a", 1, ttl).await;
        s.abandon("a").await;
        assert!(matches!(s.begin("a", 1, ttl).await, Begin::Started));
        s.complete("a", done(), ttl).await;
        s.abandon("a").await;
        assert!(matches!(s.begin("a", 1, ttl).await, Begin::Replay(_)));
    }

    #[test]
    fn expired_claims_and_results_lapse() {
        let s = MemoryStore::default();
        let t = Instant::now();
        assert!(matches!(
            s.begin_at("k", 1, Duration::from_secs(5), t),
            Begin::Started
        ));
        assert!(matches!(
            s.begin_at("k", 1, Duration::from_secs(5), t + Duration::from_secs(6)),
            Begin::Started
        ));
    }

    #[test]
    fn full_store_refuses_new_keys_until_something_expires() {
        let s = MemoryStore::new(1);
        let t = Instant::now();
        let ttl = Duration::from_secs(5);
        assert!(matches!(s.begin_at("a", 1, ttl, t), Begin::Started));
        assert!(matches!(s.begin_at("b", 1, ttl, t), Begin::Unavailable));
        assert!(matches!(
            s.begin_at("b", 1, ttl, t + Duration::from_secs(6)),
            Begin::Started
        ));
    }

    #[test]
    fn key_validation() {
        assert!(valid_key("3f9c-aa"));
        assert!(!valid_key(""));
        assert!(!valid_key("has space"));
        assert!(!valid_key(&"x".repeat(256)));
        assert!(!valid_key("ümlaut"));
    }
}
