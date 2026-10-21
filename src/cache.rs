//! Server-side response cache with request coalescing and stale-while-revalidate
//!
//! [`ResponseCache`] keeps successful `GET` responses in memory so an expensive
//! handler (database aggregate, upstream call, rendered page) runs rarely:
//!
//! * **Coalescing**: when many requests miss at once, one runs the handler and
//!   the others wait for its result, so a cold or expired entry cannot stampede
//!   the backend
//! * **Stale-while-revalidate**: for a window after expiry the old response is
//!   served immediately while a single background request refreshes it, so
//!   clients never wait for a refresh
//! * **Safe by default**: requests carrying `Authorization` or `Cookie` bypass
//!   the cache (or get their own entries with [`ResponseCache::per_user`]),
//!   responses with `Set-Cookie`, `Cache-Control: no-store` / `private`, streams
//!   and oversized bodies are never stored
//!
//! Responses get `X-Cache: HIT | MISS | STALE | COALESCED` and `Age`. The
//! lifetime comes from the response's `Cache-Control: max-age` / `s-maxage`
//! when present, otherwise from [`ResponseCache::ttl`].
//!
//! ```no_run
//! use std::time::Duration;
//! use ullun::cache::ResponseCache;
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! let cache = ResponseCache::new()
//!     .ttl(Duration::from_secs(30))
//!     .stale_while_revalidate(Duration::from_secs(120))
//!     .vary_by(["accept-language"]);
//! App::new()
//!     .middleware(cache.clone()) // keep a handle to invalidate after writes
//!     .get("/report", |_req: Request| async { Ok(Response::text("expensive")) })
//!     .post("/report/refresh", {
//!         let cache = cache.clone();
//!         move |_req: Request| {
//!             let cache = cache.clone();
//!             async move {
//!                 cache.invalidate_prefix("/report");
//!                 Ok(Response::text("ok"))
//!             }
//!         }
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Keys longer than 2 KiB are never cached. Memory use is bounded by
//! `max_entries` x `max_body` (1000 x 1 MiB by default): lower either for large responses. The cache lives in the memory of one process. A request that is not
//! cacheable (see above) is not coalesced either: each waiter then runs the
//! handler itself.

use crate::error::Result;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use bytes::Bytes;
use http::StatusCode;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Longest cache key (path, query and varied headers) that is stored
const MAX_KEY_LEN: usize = 2048;

struct Stored {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
}

impl Stored {
    fn to_response(&self, state: &str, age: Duration) -> Response {
        let mut res = Response::new(self.status).with_body(self.body.clone());
        res.headers = self.headers.clone();
        res.with_header("x-cache", state)
            .with_header("age", age.as_secs().to_string())
    }
}

struct Entry {
    stored: Arc<Stored>,
    stored_at: Instant,
    fresh_for: Duration,
}

#[derive(Clone)]
enum Flight {
    Pending,
    Done(Option<Arc<Stored>>),
}

#[derive(Default)]
struct Slot {
    path: String,
    entry: Option<Entry>,
    flight: Option<watch::Receiver<Flight>>,
    refreshing: bool,
}

struct Inner {
    slots: Mutex<HashMap<String, Slot>>,
    hasher: std::collections::hash_map::RandomState,
}

/// Cache middleware, see the [module documentation](self). Cloning gives
/// another handle to the same cache.
#[derive(Clone)]
pub struct ResponseCache {
    inner: Arc<Inner>,
    ttl: Duration,
    swr: Duration,
    max_entries: usize,
    max_body: usize,
    statuses: Vec<u16>,
    vary: Vec<String>,
    per_user: bool,
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                slots: Mutex::new(HashMap::new()),
                hasher: Default::default(),
            }),
            ttl: Duration::from_secs(60),
            swr: Duration::ZERO,
            max_entries: 1_000,
            max_body: 1024 * 1024,
            statuses: vec![200],
            vary: Vec::new(),
            per_user: false,
        }
    }

    /// Lifetime of responses that do not say otherwise (default 60 seconds)
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Serve an expired response for this long while one request refreshes it
    /// in the background (default off)
    pub fn stale_while_revalidate(mut self, window: Duration) -> Self {
        self.swr = window;
        self
    }

    /// Most cached responses (default 1000); the oldest go first
    pub fn max_entries(mut self, max: usize) -> Self {
        self.max_entries = max.max(1);
        self
    }

    /// Largest body that is cached (default 1 MiB)
    pub fn max_body(mut self, bytes: usize) -> Self {
        self.max_body = bytes;
        self
    }

    /// Status codes worth caching (default only 200)
    pub fn statuses<I: IntoIterator<Item = u16>>(mut self, statuses: I) -> Self {
        self.statuses = statuses.into_iter().collect();
        self
    }

    /// Request headers that change the response (`Accept-Language`, ...): each
    /// distinct value gets its own entry, and `Vary` is added to the response
    pub fn vary_by<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.vary = headers
            .into_iter()
            .map(|h| h.into().to_ascii_lowercase())
            .collect();
        self
    }

    /// Cache requests that carry `Authorization` or `Cookie` too, with a separate
    /// entry per credential. Only for responses that depend on nothing but the
    /// credential itself.
    pub fn per_user(mut self) -> Self {
        self.per_user = true;
        self
    }

    /// Drop every cached response whose path starts with `prefix`
    pub fn invalidate_prefix(&self, prefix: &str) {
        let mut map = self.lock();
        map.retain(|_, slot| !slot.path.starts_with(prefix));
    }

    /// Drop everything
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Number of cached responses
    pub fn len(&self) -> usize {
        self.lock().values().filter(|s| s.entry.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.inner.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn key(&self, req: &Request, scope: bool) -> String {
        let mut key = req.uri.path().to_string();
        if let Some(q) = req.uri.query() {
            key.push('?');
            key.push_str(q);
        }
        for name in &self.vary {
            key.push('|');
            if let Some(v) = req.headers.get(name.as_str()).and_then(|v| v.to_str().ok()) {
                key.push_str(v);
            }
        }
        if scope {
            let mut h = self.inner.hasher.build_hasher();
            for name in ["authorization", "cookie"] {
                req.headers.get(name).map(|v| v.as_bytes()).hash(&mut h);
            }
            key.push_str(&format!("|u{:016x}", h.finish()));
        }
        key
    }

    /// Whether and for how long a response may be stored
    fn lifetime(&self, res: &Response) -> Option<Duration> {
        if !self.statuses.contains(&res.status.as_u16())
            || res.is_streaming()
            || res.has_cookies()
            || res.headers.contains_key(http::header::SET_COOKIE)
            || res.body.len() > self.max_body
        {
            return None;
        }
        let mut max_age = None;
        if let Some(cc) = res
            .headers
            .get(http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
        {
            for directive in cc.split(',').map(|d| d.trim().to_ascii_lowercase()) {
                match directive.as_str() {
                    "no-store" | "private" | "no-cache" => return None,
                    d => {
                        if let Some(n) = d
                            .strip_prefix("s-maxage=")
                            .or_else(|| d.strip_prefix("max-age="))
                        {
                            if let Ok(secs) = n.trim_matches('"').parse::<u64>() {
                                max_age.get_or_insert(secs);
                                if d.starts_with("s-maxage=") {
                                    max_age = Some(secs);
                                }
                            }
                        }
                    }
                }
            }
        }
        match max_age {
            Some(0) => None,
            Some(secs) => Some(Duration::from_secs(secs)),
            None => Some(self.ttl),
        }
    }

    fn snapshot(&self, res: &Response) -> Stored {
        let mut headers = res.headers.clone();
        // freshness is tracked here; a stale Age from upstream would mislead
        headers.remove(http::header::AGE);
        headers.remove("x-cache");
        Stored {
            status: res.status,
            headers,
            body: res.body.clone(),
        }
    }

    fn store(&self, key: &str, path: &str, stored: Arc<Stored>, fresh_for: Duration) {
        let mut map = self.lock();
        let others = |map: &HashMap<String, Slot>| map.len() - usize::from(map.contains_key(key));
        if others(&map) >= self.max_entries {
            let now = Instant::now();
            let swr = self.swr;
            map.retain(|k, s| {
                k == key
                    || s.flight.is_some()
                    || s.entry
                        .as_ref()
                        .is_some_and(|e| now.duration_since(e.stored_at) < e.fresh_for + swr)
            });
            while others(&map) >= self.max_entries {
                let oldest = map
                    .iter()
                    .filter(|(k, s)| *k != key && s.flight.is_none() && s.entry.is_some())
                    .min_by_key(|(_, s)| s.entry.as_ref().map(|e| e.stored_at))
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(k) => {
                        map.remove(&k);
                    }
                    None => break,
                }
            }
        }
        let slot = map.entry(key.to_string()).or_default();
        slot.path = path.to_string();
        slot.entry = Some(Entry {
            stored,
            stored_at: Instant::now(),
            fresh_for,
        });
    }

    fn finish_flight(&self, key: &str) {
        let mut map = self.lock();
        if let Some(slot) = map.get_mut(key) {
            slot.flight = None;
            if slot.entry.is_none() {
                map.remove(key);
            }
        }
    }

    fn end_refresh(&self, key: &str) {
        let mut map = self.lock();
        if let Some(slot) = map.get_mut(key) {
            slot.refreshing = false;
            if slot.entry.is_none() && slot.flight.is_none() {
                map.remove(key);
            }
        }
    }
}

/// Ends a flight for waiters even if the leader's future is dropped
struct Leader {
    cache: ResponseCache,
    key: String,
    tx: watch::Sender<Flight>,
    done: bool,
}

impl Leader {
    fn finish(mut self, result: Option<Arc<Stored>>) {
        self.done = true;
        let _ = self.tx.send(Flight::Done(result));
        self.cache.finish_flight(&self.key);
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.tx.send(Flight::Done(None));
            self.cache.finish_flight(&self.key);
        }
    }
}

enum Plan {
    Hit(Arc<Stored>, Duration),
    Stale(Arc<Stored>, Duration, bool),
    Wait(watch::Receiver<Flight>),
    Lead(Leader),
}

impl ResponseCache {
    fn plan(&self, key: &str, path: &str) -> Plan {
        let mut map = self.lock();
        let slot = map.entry(key.to_string()).or_default();
        if slot.path.is_empty() {
            slot.path = path.to_string();
        }
        if let Some(entry) = &slot.entry {
            let age = entry.stored_at.elapsed();
            if age < entry.fresh_for {
                return Plan::Hit(entry.stored.clone(), age);
            }
            if age < entry.fresh_for + self.swr {
                let start_refresh = !slot.refreshing && slot.flight.is_none();
                if start_refresh {
                    slot.refreshing = true;
                }
                return Plan::Stale(entry.stored.clone(), age, start_refresh);
            }
        }
        if let Some(rx) = &slot.flight {
            return Plan::Wait(rx.clone());
        }
        let (tx, rx) = watch::channel(Flight::Pending);
        slot.flight = Some(rx);
        slot.entry = None;
        Plan::Lead(Leader {
            cache: self.clone(),
            key: key.to_string(),
            tx,
            done: false,
        })
    }
}

#[async_trait]
impl Middleware for ResponseCache {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let credentialed = req.headers.contains_key(http::header::AUTHORIZATION)
            || req.headers.contains_key(http::header::COOKIE);
        if req.method != http::Method::GET || (credentialed && !self.per_user) {
            return next.run(req).await;
        }
        let key = self.key(&req, credentialed);
        if key.len() > MAX_KEY_LEN {
            // keys are kept in memory: a request cannot make us hold a huge one
            return next.run(req).await;
        }
        let path = req.uri.path().to_string();
        let finish = |mut res: Response| {
            for v in &self.vary {
                crate::cors::add_vary(&mut res, v);
            }
            res
        };

        // A waiter whose leader produced nothing storable (error, private
        // response) leads once itself; after that it just runs the handler.
        let mut waited = false;
        loop {
            match self.plan(&key, &path) {
                Plan::Hit(stored, age) => return Ok(finish(stored.to_response("HIT", age))),
                Plan::Stale(stored, age, refresh) => {
                    if refresh {
                        self.spawn_refresh(key, path, next, req.duplicate());
                    }
                    return Ok(finish(stored.to_response("STALE", age)));
                }
                Plan::Wait(_) if waited => return next.run(req).await,
                Plan::Wait(mut rx) => {
                    let outcome = loop {
                        if let Flight::Done(result) = &*rx.borrow_and_update() {
                            break Some(result.clone());
                        }
                        if rx.changed().await.is_err() {
                            break None;
                        }
                    };
                    match outcome.flatten() {
                        Some(stored) => {
                            return Ok(finish(stored.to_response("COALESCED", Duration::ZERO)))
                        }
                        None => waited = true,
                    }
                }
                Plan::Lead(leader) => {
                    let res = next.run(req).await.unwrap_or_else(Response::from_error);
                    return Ok(match self.lifetime(&res) {
                        Some(fresh_for) => {
                            let stored = Arc::new(self.snapshot(&res));
                            self.store(&key, &path, stored.clone(), fresh_for);
                            leader.finish(Some(stored));
                            finish(res.with_header("x-cache", "MISS"))
                        }
                        None => {
                            leader.finish(None);
                            res.with_header("x-cache", "BYPASS")
                        }
                    });
                }
            }
        }
    }
}

impl ResponseCache {
    /// Refresh a stale entry without making any client wait
    fn spawn_refresh(&self, key: String, path: String, next: Next, req: Request) {
        let cache = self.clone();
        tokio::spawn(async move {
            let res = next.run(req).await.unwrap_or_else(Response::from_error);
            match cache.lifetime(&res) {
                Some(fresh_for) => {
                    let stored = Arc::new(cache.snapshot(&res));
                    cache.store(&key, &path, stored, fresh_for);
                }
                None => {
                    // do not keep serving something the origin no longer allows
                    if let Some(slot) = cache.lock().get_mut(&key) {
                        slot.entry = None;
                    }
                }
            }
            cache.end_refresh(&key);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(status: u16, cc: Option<&str>) -> Response {
        let mut r = Response::new(StatusCode::from_u16(status).unwrap()).with_body("x");
        if let Some(cc) = cc {
            r = r.with_header("cache-control", cc);
        }
        r
    }

    #[test]
    fn lifetime_follows_cache_control() {
        let c = ResponseCache::new().ttl(Duration::from_secs(60));
        assert_eq!(c.lifetime(&res(200, None)), Some(Duration::from_secs(60)));
        assert_eq!(
            c.lifetime(&res(200, Some("max-age=5"))),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            c.lifetime(&res(200, Some("public, max-age=5, s-maxage=9"))),
            Some(Duration::from_secs(9))
        );
        for no in ["no-store", "private", "no-cache", "max-age=0"] {
            assert_eq!(c.lifetime(&res(200, Some(no))), None, "{no}");
        }
        assert_eq!(c.lifetime(&res(404, None)), None);
        assert_eq!(
            c.lifetime(&res(200, None).with_header("set-cookie", "a=b")),
            None
        );
    }

    #[test]
    fn big_bodies_are_not_stored() {
        let c = ResponseCache::new().max_body(1);
        assert_eq!(c.lifetime(&Response::ok().with_body("xx")), None);
    }

    #[test]
    fn statuses_are_configurable() {
        let c = ResponseCache::new().statuses([200, 404]);
        assert!(c.lifetime(&res(404, None)).is_some());
    }
}
