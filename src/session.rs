//! Cookie-based server-side sessions
//!
//! The browser only holds an unguessable session id (256 bits from the OS
//! CSPRNG) in an `HttpOnly`, `SameSite=Lax` cookie; the data lives in a
//! [`SessionStore`]. Anonymous visitors that never write to the session get no
//! cookie at all.
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::session::SessionLayer;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(SessionLayer::new())
//!     .post("/login", |req: Request| async move {
//!         let session = req.session()?;
//!         session.renew();                               // new id on privilege change: no fixation
//!         session.set("user", "ann")?;
//!         Ok(Response::redirect("/"))
//!     })
//!     .get("/", |req: Request| async move {
//!         let user: Option<String> = req.session()?.get("user");
//!         Ok(Response::text(format!("hello {}", user.unwrap_or_else(|| "stranger".into()))))
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Defaults: cookie `__Host-sid` (needs HTTPS: `Secure`, `Path=/`, no `Domain`),
//! 30 minutes idle and 12 hours absolute lifetime. For plain-HTTP development
//! use [`SessionLayer::insecure_cookies`]. Concurrent requests of one session
//! are not merged: the last response to finish wins, so keep session data small
//! and put mutable state in your database.

use crate::cookies::{Cookie, SameSite};
use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::security::random_token;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const FLASH_KEY: &str = "_flash";
const CSRF_KEY: &str = "_csrf";

/// What a store keeps per session
#[derive(Debug, Clone, Default)]
pub struct SessionRecord {
    pub data: Map<String, Value>,
    pub created_at: u64,
    pub last_seen: u64,
}

/// Where session data lives. Implement it for Redis, a database, ...
#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    async fn load(&self, id: &str) -> Option<SessionRecord>;
    async fn save(&self, id: &str, record: &SessionRecord);
    async fn delete(&self, id: &str);
}

/// In-process store with a size cap; sessions are lost on restart and not
/// shared between instances
pub struct MemoryStore {
    sessions: Mutex<HashMap<String, SessionRecord>>,
    max_sessions: usize,
}

impl MemoryStore {
    /// At most 100 000 sessions
    pub fn new() -> Self {
        Self::with_capacity(100_000)
    }

    /// When full, the least recently seen sessions are dropped to make room
    pub fn with_capacity(max_sessions: usize) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            max_sessions: max_sessions.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, SessionRecord>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for MemoryStore {
    async fn load(&self, id: &str) -> Option<SessionRecord> {
        self.lock().get(id).cloned()
    }

    async fn save(&self, id: &str, record: &SessionRecord) {
        let mut map = self.lock();
        if map.len() >= self.max_sessions && !map.contains_key(id) {
            // evict the oldest tenth (at least one) by last use
            let mut by_age: Vec<(u64, String)> =
                map.iter().map(|(k, v)| (v.last_seen, k.clone())).collect();
            by_age.sort();
            for (_, key) in by_age.into_iter().take((self.max_sessions / 10).max(1)) {
                map.remove(&key);
            }
        }
        map.insert(id.to_string(), record.clone());
    }

    async fn delete(&self, id: &str) {
        self.lock().remove(id);
    }
}

#[derive(Default)]
struct Inner {
    /// Id the request arrived with (a valid, unexpired session), if any
    id: Option<String>,
    data: Map<String, Value>,
    created_at: u64,
    last_seen: u64,
    dirty: bool,
    renew: bool,
    destroy: bool,
}

/// The session of the current request; cheap to clone
#[derive(Clone)]
pub struct Session {
    inner: Arc<Mutex<Inner>>,
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A value, if present and of the requested type
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let inner = self.lock();
        inner
            .data
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.lock().data.contains_key(key)
    }

    /// Store a value; creates the session (and its cookie) if needed
    pub fn set<T: Serialize>(&self, key: &str, value: T) -> Result<()> {
        let value = serde_json::to_value(value)
            .map_err(|e| Error::internal(format!("Session value: {}", e)))?;
        let mut inner = self.lock();
        inner.data.insert(key.to_string(), value);
        inner.dirty = true;
        Ok(())
    }

    pub fn remove(&self, key: &str) {
        let mut inner = self.lock();
        if inner.data.remove(key).is_some() {
            inner.dirty = true;
        }
    }

    /// Drop all data but keep the session
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.data.clear();
        inner.dirty = true;
    }

    /// Give the session a fresh id, keeping its data. Call it whenever the
    /// privilege level changes (login, logout, role change) so that an id an
    /// attacker planted or learned earlier becomes useless.
    pub fn renew(&self) {
        let mut inner = self.lock();
        inner.renew = true;
        inner.dirty = true;
    }

    /// Delete the session and its cookie (logout)
    pub fn destroy(&self) {
        let mut inner = self.lock();
        inner.destroy = true;
        inner.data.clear();
    }

    /// Queue a message for the next page (survives one redirect)
    pub fn flash(&self, message: impl Into<String>) -> Result<()> {
        let mut inner = self.lock();
        let list = inner
            .data
            .entry(FLASH_KEY.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(list) = list {
            list.push(Value::String(message.into()));
        }
        inner.dirty = true;
        Ok(())
    }

    /// Messages queued with [`flash`](Session::flash); each is returned once
    pub fn take_flash(&self) -> Vec<String> {
        let mut inner = self.lock();
        match inner.data.remove(FLASH_KEY) {
            Some(Value::Array(list)) => {
                inner.dirty = true;
                list.into_iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// The CSRF token of this session, created on first use. Rotates with
    /// [`renew`](Session::renew)'s data like any other value. See [`crate::csrf`].
    pub fn csrf_token(&self) -> Result<String> {
        if let Some(t) = self.get::<String>(CSRF_KEY) {
            return Ok(t);
        }
        let token = random_token(32)?;
        self.set(CSRF_KEY, &token)?;
        Ok(token)
    }

    /// True when this request belongs to a stored session
    pub fn is_established(&self) -> bool {
        self.lock().id.is_some()
    }
}

impl Request {
    /// The session of this request (needs the [`SessionLayer`] middleware;
    /// answers `500` otherwise)
    pub fn session(&self) -> Result<&Session> {
        self.extensions
            .get::<Session>()
            .ok_or_else(|| Error::internal("SessionLayer middleware is not installed"))
    }
}

/// Session middleware
pub struct SessionLayer {
    store: Arc<dyn SessionStore>,
    cookie_name: String,
    secure: bool,
    idle_ttl: u64,
    absolute_ttl: u64,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl SessionLayer {
    /// In-memory store, `__Host-sid` cookie, 30 min idle / 12 h absolute
    pub fn new() -> Self {
        Self::with_store(MemoryStore::new())
    }

    pub fn with_store(store: impl SessionStore) -> Self {
        Self {
            store: Arc::new(store),
            cookie_name: "__Host-sid".into(),
            secure: true,
            idle_ttl: 30 * 60,
            absolute_ttl: 12 * 3600,
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        }
    }

    /// Cookie name (default `__Host-sid`, only valid together with `Secure`)
    pub fn cookie_name(mut self, name: impl Into<String>) -> Self {
        self.cookie_name = name.into();
        self
    }

    /// For plain-HTTP development: no `Secure` flag and the cookie name `sid`
    /// (the `__Host-` prefix is rejected by browsers without `Secure`)
    pub fn insecure_cookies(mut self) -> Self {
        self.secure = false;
        if self.cookie_name.starts_with("__Host-") {
            self.cookie_name = "sid".into();
        }
        self
    }

    /// A session ends after this many seconds without a request
    pub fn idle_timeout(mut self, secs: u64) -> Self {
        self.idle_ttl = secs.max(1);
        self
    }

    /// A session ends this many seconds after it was created, however active
    pub fn absolute_timeout(mut self, secs: u64) -> Self {
        self.absolute_ttl = secs.max(1);
        self
    }

    /// Replace the time source (tests)
    pub fn clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    fn cookie(&self, value: &str, max_age: i64) -> Cookie {
        let c = Cookie::new(&self.cookie_name, value)
            .path("/")
            .max_age(max_age)
            .http_only()
            .same_site(SameSite::Lax);
        if self.secure {
            c.secure()
        } else {
            c
        }
    }
}

impl Default for SessionLayer {
    fn default() -> Self {
        Self::new()
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[async_trait]
impl Middleware for SessionLayer {
    async fn handle(&self, mut req: Request, next: Next) -> Result<Response> {
        let now = (self.clock)();
        let presented = req
            .cookies()
            .get(&self.cookie_name)
            .filter(|id| valid_id(id))
            .map(str::to_owned);

        let mut inner = Inner {
            created_at: now,
            ..Inner::default()
        };
        let mut expired_cookie = false;
        if let Some(id) = presented {
            match self.store.load(&id).await {
                Some(rec)
                    if now.saturating_sub(rec.last_seen) < self.idle_ttl
                        && now.saturating_sub(rec.created_at) < self.absolute_ttl =>
                {
                    inner.id = Some(id);
                    inner.data = rec.data;
                    inner.created_at = rec.created_at;
                    inner.last_seen = rec.last_seen;
                }
                Some(_) => {
                    self.store.delete(&id).await;
                    expired_cookie = true;
                }
                None => expired_cookie = true, // unknown id: forget the cookie
            }
        }
        let session = Session {
            inner: Arc::new(Mutex::new(inner)),
        };
        req.extensions.insert(session.clone());

        let mut res = match next.run(req).await {
            Ok(res) => res,
            Err(err) => Response::from_error(err),
        };

        let (old_id, data, created_at, last_seen, dirty, renew, destroy) = {
            let i = session.lock();
            (
                i.id.clone(),
                i.data.clone(),
                i.created_at,
                i.last_seen,
                i.dirty,
                i.renew,
                i.destroy,
            )
        };

        if destroy {
            if let Some(id) = &old_id {
                self.store.delete(id).await;
            }
            if old_id.is_some() || expired_cookie {
                res = res.cookie(self.cookie("", 0));
            }
            return Ok(res);
        }

        match (old_id, dirty) {
            // changed (or created): persist, under a new id when renewed or new
            (old, true) => {
                let id = match (&old, renew) {
                    (Some(old), false) => old.clone(),
                    (old, _) => {
                        if let Some(old) = old {
                            self.store.delete(old).await;
                        }
                        random_token(32)?
                    }
                };
                self.store
                    .save(
                        &id,
                        &SessionRecord {
                            data,
                            // a renewed session starts a fresh absolute lifetime
                            created_at: if renew { now } else { created_at },
                            last_seen: now,
                        },
                    )
                    .await;
                let remaining = self
                    .absolute_ttl
                    .saturating_sub(if renew {
                        0
                    } else {
                        now.saturating_sub(created_at)
                    })
                    .min(self.idle_ttl);
                res = res.cookie(self.cookie(&id, remaining as i64));
            }
            // unchanged but alive: keep the idle timer running
            (Some(id), false) => {
                if now.saturating_sub(last_seen) >= self.idle_ttl / 4 {
                    self.store
                        .save(
                            &id,
                            &SessionRecord {
                                data,
                                created_at,
                                last_seen: now,
                            },
                        )
                        .await;
                    let remaining = self
                        .absolute_ttl
                        .saturating_sub(now.saturating_sub(created_at))
                        .min(self.idle_ttl);
                    res = res.cookie(self.cookie(&id, remaining as i64));
                }
            }
            (None, false) => {
                if expired_cookie {
                    res = res.cookie(self.cookie("", 0));
                }
            }
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_format() {
        assert!(valid_id(&"a1".repeat(32)));
        assert!(!valid_id(&"A1".repeat(32)));
        assert!(!valid_id(&"a1".repeat(31)));
        assert!(!valid_id(""));
        assert!(!valid_id(&format!("{}; Path=/", "a".repeat(40))));
    }

    #[tokio::test]
    async fn memory_store_evicts_the_least_recently_seen() {
        let store = MemoryStore::with_capacity(3);
        for (id, seen) in [("a", 10), ("b", 20), ("c", 30)] {
            store
                .save(
                    id,
                    &SessionRecord {
                        last_seen: seen,
                        ..Default::default()
                    },
                )
                .await;
        }
        store
            .save(
                "d",
                &SessionRecord {
                    last_seen: 40,
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(store.len(), 3);
        assert!(store.load("a").await.is_none());
        assert!(store.load("d").await.is_some());
        // overwriting an existing id never evicts
        store
            .save(
                "d",
                &SessionRecord {
                    last_seen: 50,
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(store.len(), 3);
        store.delete("b").await;
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn session_values_and_flash() {
        let s = Session {
            inner: Arc::new(Mutex::new(Inner::default())),
        };
        assert!(s.get::<String>("k").is_none());
        s.set("k", "v").unwrap();
        s.set("n", 5u32).unwrap();
        assert_eq!(s.get::<String>("k").as_deref(), Some("v"));
        assert_eq!(s.get::<u32>("n"), Some(5));
        assert_eq!(s.get::<u32>("k"), None); // wrong type
        s.remove("n");
        assert!(!s.contains("n"));

        s.flash("one").unwrap();
        s.flash("two").unwrap();
        assert_eq!(s.take_flash(), ["one", "two"]);
        assert!(s.take_flash().is_empty());

        let t1 = s.csrf_token().unwrap();
        assert_eq!(t1.len(), 64);
        assert_eq!(s.csrf_token().unwrap(), t1);
        s.clear();
        assert_ne!(s.csrf_token().unwrap(), t1);
    }
}
