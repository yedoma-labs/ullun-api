//! Minimal Redis client and Redis-backed stores (feature `redis`)
//!
//! Sessions, rate limits and idempotency keys live in process memory by
//! default, so a second instance sees none of them. With this feature the same
//! middleware keeps its state in Redis, shared by every instance:
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::redis::{Redis, RedisSessionStore};
//! use ullun::session::SessionLayer;
//!
//! # async fn run() -> ullun::error::Result<()> {
//! let redis = Redis::from_url("redis://127.0.0.1:6379/0")?;
//! App::new()
//!     .middleware(SessionLayer::with_store(RedisSessionStore::new(redis)))
//!     .run("127.0.0.1:3000")
//!     .await
//! # }
//! ```
//!
//! The client speaks RESP2 over plain TCP with a small connection pool and
//! brings no dependencies. It supports `redis://[user:password@]host[:port][/db]`
//! and is meant for the stores here; for anything else use a full client and
//! implement the store traits ([`SessionStore`], [`IdempotencyStore`],
//! [`RateLimitStore`]) yourself. TLS (`rediss://`) is not supported: terminate it
//! with a local proxy or keep Redis on a private network.
//!
//! Failure behavior is chosen per store: a session store that cannot reach
//! Redis behaves as if the session does not exist, the rate limiter fails open
//! (requests are let through), idempotency fails closed (`503`) because
//! running a payment twice is worse than refusing it.

use crate::error::{Error, Result};
use crate::idempotency::{Begin, IdempotencyStore, StoredResponse};
use crate::rate_limit::{Decision, RateLimitStore};
use crate::session::{SessionRecord, SessionStore};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const MAX_BULK: usize = 64 * 1024 * 1024;

/// A reply from Redis
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Int(i64),
    Status(String),
    Bulk(Vec<u8>),
    Array(Vec<Value>),
    /// An error reply (`-ERR ...`); [`Redis::run`] turns it into an `Err`
    Error(String),
}

impl Value {
    /// The bytes of a bulk string, `None` for nil
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Value::Bulk(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Value::Status(s) if s == "OK")
    }
}

/// A command under construction
#[derive(Debug, Clone)]
pub struct Cmd {
    args: Vec<Vec<u8>>,
}

impl Cmd {
    pub fn new(name: &str) -> Self {
        Self {
            args: vec![name.as_bytes().to_vec()],
        }
    }

    pub fn arg(mut self, arg: impl AsRef<[u8]>) -> Self {
        self.args.push(arg.as_ref().to_vec());
        self
    }

    pub fn int(self, n: i64) -> Self {
        self.arg(n.to_string())
    }

    /// RESP encoding as sent on the wire
    pub fn encode(&self) -> Vec<u8> {
        let mut out = format!("*{}\r\n", self.args.len()).into_bytes();
        for a in &self.args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a);
            out.extend_from_slice(b"\r\n");
        }
        out
    }
}

struct Conn {
    io: BufReader<TcpStream>,
}

impl Conn {
    async fn read_line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        let n = self.io.read_until(b'\n', &mut line).await.map_err(io_err)?;
        if n == 0 || !line.ends_with(b"\r\n") {
            return Err(Error::internal("Redis closed the connection"));
        }
        line.truncate(line.len() - 2);
        Ok(line)
    }

    fn read_value<'a>(
        &'a mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let line = self.read_line().await?;
            let (kind, rest) = line
                .split_first()
                .ok_or_else(|| Error::internal("Empty Redis reply"))?;
            let text = || String::from_utf8_lossy(rest).into_owned();
            let number = || -> Result<i64> {
                text()
                    .parse()
                    .map_err(|_| Error::internal("Malformed Redis reply"))
            };
            match kind {
                b'+' => Ok(Value::Status(text())),
                b'-' => Ok(Value::Error(text())),
                b':' => Ok(Value::Int(number()?)),
                b'$' => {
                    let len = number()?;
                    if len < 0 {
                        return Ok(Value::Nil);
                    }
                    let len = len as usize;
                    if len > MAX_BULK {
                        return Err(Error::internal("Redis reply too large"));
                    }
                    let mut buf = vec![0u8; len + 2];
                    self.io.read_exact(&mut buf).await.map_err(io_err)?;
                    buf.truncate(len);
                    Ok(Value::Bulk(buf))
                }
                b'*' => {
                    let len = number()?;
                    if len < 0 {
                        return Ok(Value::Nil);
                    }
                    let mut items = Vec::with_capacity((len as usize).min(1024));
                    for _ in 0..len {
                        items.push(self.read_value().await?);
                    }
                    Ok(Value::Array(items))
                }
                _ => Err(Error::internal("Unknown Redis reply type")),
            }
        })
    }
}

fn io_err(e: std::io::Error) -> Error {
    Error::internal(format!("Redis I/O: {e}"))
}

/// A Redis server and a pool of connections to it
pub struct Redis {
    addr: String,
    username: Option<String>,
    password: Option<String>,
    db: u32,
    timeout: Duration,
    max_idle: usize,
    idle: Mutex<VecDeque<Conn>>,
}

impl std::fmt::Debug for Redis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redis")
            .field("addr", &self.addr)
            .field("db", &self.db)
            .finish_non_exhaustive()
    }
}

impl Redis {
    /// `redis://[user:password@]host[:port][/db]`; no connection is made yet
    pub fn from_url(url: &str) -> Result<Self> {
        let bad =
            || Error::internal("Redis URL must look like redis://[user:password@]host[:port][/db]");
        let rest = url.strip_prefix("redis://").ok_or_else(bad)?;
        let (authority, db) = match rest.split_once('/') {
            Some((a, d)) if !d.is_empty() => (a, d.parse::<u32>().map_err(|_| bad())?),
            Some((a, _)) => (a, 0),
            None => (rest, 0),
        };
        let (credentials, host) = match authority.rsplit_once('@') {
            Some((c, h)) => (Some(c), h),
            None => (None, authority),
        };
        if host.is_empty() {
            return Err(bad());
        }
        let (username, password) = match credentials {
            None => (None, None),
            Some(c) => match c.split_once(':') {
                Some((u, p)) => ((!u.is_empty()).then(|| decode(u)), Some(decode(p))),
                None => (None, Some(decode(c))),
            },
        };
        let addr = if host.contains(':') && !host.starts_with('[') || host.contains("]:") {
            host.to_string()
        } else {
            format!("{host}:6379")
        };
        Ok(Self {
            addr,
            username,
            password,
            db,
            timeout: Duration::from_secs(5),
            max_idle: 16,
            idle: Mutex::new(VecDeque::new()),
        })
    }

    /// Connect, read and write timeout per command (default 5 seconds)
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Idle connections kept for reuse (default 16)
    pub fn max_idle(mut self, max: usize) -> Self {
        self.max_idle = max;
        self
    }

    async fn connect(&self) -> Result<Conn> {
        let stream = TcpStream::connect(&self.addr).await.map_err(io_err)?;
        let _ = stream.set_nodelay(true);
        let mut conn = Conn {
            io: BufReader::new(stream),
        };
        if let Some(password) = &self.password {
            let mut auth = Cmd::new("AUTH");
            if let Some(user) = &self.username {
                auth = auth.arg(user);
            }
            expect_ok(exchange(&mut conn, &[auth.arg(password)]).await?)?;
        }
        if self.db != 0 {
            expect_ok(exchange(&mut conn, &[Cmd::new("SELECT").int(i64::from(self.db))]).await?)?;
        }
        Ok(conn)
    }

    fn take_idle(&self) -> Option<Conn> {
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    fn put_idle(&self, conn: Conn) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < self.max_idle {
            idle.push_back(conn);
        }
    }

    /// Send commands in one round trip and collect their replies. Error replies
    /// stay in the result as [`Value::Error`].
    pub async fn pipeline(&self, cmds: &[Cmd]) -> Result<Vec<Value>> {
        let attempt = |mut conn: Conn| async move {
            let replies = exchange(&mut conn, cmds).await?;
            Ok::<_, Error>((conn, replies))
        };
        tokio::time::timeout(self.timeout, async {
            // a pooled connection may have been closed by the server meanwhile:
            // fall back to a fresh one once
            if let Some(conn) = self.take_idle() {
                if let Ok((conn, replies)) = attempt(conn).await {
                    self.put_idle(conn);
                    return Ok(replies);
                }
            }
            let (conn, replies) = attempt(self.connect().await?).await?;
            self.put_idle(conn);
            Ok(replies)
        })
        .await
        .map_err(|_| Error::internal("Redis timed out"))?
    }

    /// Run one command; an error reply becomes `Err`
    pub async fn run(&self, cmd: Cmd) -> Result<Value> {
        match self.pipeline(std::slice::from_ref(&cmd)).await?.pop() {
            Some(Value::Error(e)) => Err(Error::internal(format!("Redis: {e}"))),
            Some(v) => Ok(v),
            None => Err(Error::internal("Redis sent no reply")),
        }
    }
}

async fn exchange(conn: &mut Conn, cmds: &[Cmd]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for c in cmds {
        out.extend_from_slice(&c.encode());
    }
    conn.io.write_all(&out).await.map_err(io_err)?;
    let mut replies = Vec::with_capacity(cmds.len());
    for _ in cmds {
        replies.push(conn.read_value().await?);
    }
    Ok(replies)
}

fn expect_ok(mut replies: Vec<Value>) -> Result<()> {
    match replies.pop() {
        Some(v) if v.is_ok() => Ok(()),
        Some(Value::Error(e)) => Err(Error::internal(format!("Redis: {e}"))),
        _ => Err(Error::internal("Unexpected Redis reply")),
    }
}

fn decode(s: &str) -> String {
    urlencoding::decode(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX).max(1)
}

// ---- sessions ---------------------------------------------------------------

/// [`SessionStore`] in Redis: one JSON string per session with an expiry
pub struct RedisSessionStore {
    redis: Redis,
    prefix: String,
    ttl: Duration,
}

impl RedisSessionStore {
    /// Keys `ullun:session:<id>`, expiring 24 hours after the last save
    pub fn new(redis: Redis) -> Self {
        Self {
            redis,
            prefix: "ullun:session:".into(),
            ttl: Duration::from_secs(24 * 3600),
        }
    }

    /// Key prefix, to share a Redis between applications
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Expiry after the last save. Set it to at least the session layer's
    /// absolute lifetime.
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    fn key(&self, id: &str) -> String {
        format!("{}{}", self.prefix, id)
    }
}

#[async_trait]
impl SessionStore for RedisSessionStore {
    async fn load(&self, id: &str) -> Option<SessionRecord> {
        let raw = self
            .redis
            .run(Cmd::new("GET").arg(self.key(id)))
            .await
            .ok()?
            .into_bytes()?;
        let doc: serde_json::Value = serde_json::from_slice(&raw).ok()?;
        Some(SessionRecord {
            data: doc.get("d")?.as_object()?.clone(),
            created_at: doc.get("c")?.as_u64()?,
            last_seen: doc.get("l")?.as_u64()?,
        })
    }

    async fn save(&self, id: &str, record: &SessionRecord) {
        let doc = serde_json::json!({
            "d": record.data,
            "c": record.created_at,
            "l": record.last_seen,
        });
        let cmd = Cmd::new("SET")
            .arg(self.key(id))
            .arg(doc.to_string())
            .arg("PX")
            .int(millis(self.ttl));
        if let Err(e) = self.redis.run(cmd).await {
            eprintln!("Saving session to Redis failed: {}", e.message());
        }
    }

    async fn delete(&self, id: &str) {
        let _ = self.redis.run(Cmd::new("DEL").arg(self.key(id))).await;
    }
}

// ---- rate limits ------------------------------------------------------------

/// [`RateLimitStore`] in Redis. Fails open: when Redis is unreachable requests
/// are allowed.
pub struct RedisRateLimitStore {
    redis: Redis,
    prefix: String,
}

impl RedisRateLimitStore {
    pub fn new(redis: Redis) -> Self {
        Self {
            redis,
            prefix: "ullun:rl:".into(),
        }
    }

    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    async fn fixed(&self, key: &str, limit: u32, window: Duration) -> Result<Decision> {
        let k = format!("{}{}", self.prefix, key);
        let window_ms = millis(window);
        // create the counter with its expiry first, so INCR never leaves one without
        let replies = self
            .redis
            .pipeline(&[
                Cmd::new("SET")
                    .arg(&k)
                    .arg("0")
                    .arg("NX")
                    .arg("PX")
                    .int(window_ms),
                Cmd::new("INCR").arg(&k),
                Cmd::new("PTTL").arg(&k),
            ])
            .await?;
        let count = replies.get(1).and_then(Value::as_int).unwrap_or(1).max(1) as u64;
        let mut ttl = replies.get(2).and_then(Value::as_int).unwrap_or(window_ms);
        if ttl < 0 {
            // the counter expired between SET and INCR: restart its window
            let _ = self
                .redis
                .run(Cmd::new("PEXPIRE").arg(&k).int(window_ms))
                .await;
            ttl = window_ms;
        }
        Ok(Decision {
            allowed: count <= u64::from(limit),
            limit,
            remaining: u64::from(limit).saturating_sub(count) as u32,
            reset_secs: ((ttl as u64).div_ceil(1000)).max(1),
        })
    }

    async fn sliding(&self, key: &str, limit: u32, window: Duration) -> Result<Decision> {
        let window_ms = millis(window) as u64;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (index, elapsed) = (now_ms / window_ms, now_ms % window_ms);
        let cur = format!("{}{}:{}", self.prefix, key, index);
        let prev = format!("{}{}:{}", self.prefix, key, index.wrapping_sub(1));
        let replies = self
            .redis
            .pipeline(&[
                Cmd::new("INCR").arg(&cur),
                Cmd::new("PEXPIRE").arg(&cur).int((window_ms * 2) as i64),
                Cmd::new("GET").arg(&prev),
            ])
            .await?;
        let count = replies.first().and_then(Value::as_int).unwrap_or(1).max(1) as f64;
        let previous = match replies.get(2) {
            Some(Value::Bulk(b)) => String::from_utf8_lossy(b).parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        };
        let weight = 1.0 - elapsed as f64 / window_ms as f64;
        let estimate = previous * weight + count;
        let limit_f = f64::from(limit);
        let reset_secs = ((window_ms - elapsed).div_ceil(1000)).max(1);
        if estimate <= limit_f {
            return Ok(Decision {
                allowed: true,
                limit,
                remaining: (limit_f - estimate).floor().max(0.0) as u32,
                reset_secs,
            });
        }
        // denied requests must not count against the client
        let _ = self.redis.run(Cmd::new("DECR").arg(&cur)).await;
        Ok(Decision {
            allowed: false,
            limit,
            remaining: 0,
            reset_secs,
        })
    }
}

#[async_trait]
impl RateLimitStore for RedisRateLimitStore {
    async fn hit(&self, key: &str, limit: u32, window: Duration, sliding: bool) -> Decision {
        let result = if sliding {
            self.sliding(key, limit, window).await
        } else {
            self.fixed(key, limit, window).await
        };
        result.unwrap_or_else(|e| {
            eprintln!("Rate limit store unavailable, allowing: {}", e.message());
            Decision {
                allowed: true,
                limit,
                remaining: limit,
                reset_secs: window.as_secs().max(1),
            }
        })
    }
}

// ---- idempotency ------------------------------------------------------------

/// [`IdempotencyStore`] in Redis. Pair it with
/// [`Idempotency::shared_seed`](crate::idempotency::Idempotency::shared_seed) so
/// all instances hash scopes and payloads alike. Fails closed: when Redis is
/// unreachable requests get `503`.
pub struct RedisIdempotencyStore {
    redis: Redis,
    prefix: String,
}

const RUNNING: u8 = 0;
const DONE: u8 = 1;

fn encode_entry(fingerprint: u64, response: Option<&StoredResponse>) -> Vec<u8> {
    let mut out = fingerprint.to_be_bytes().to_vec();
    let Some(res) = response else {
        out.push(RUNNING);
        return out;
    };
    out.push(DONE);
    out.extend_from_slice(&res.status.to_be_bytes());
    out.extend_from_slice(&(res.headers.len() as u32).to_be_bytes());
    for (name, value) in &res.headers {
        out.extend_from_slice(&(name.len() as u32).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
    }
    out.extend_from_slice(&res.body);
    out
}

fn decode_entry(raw: &[u8]) -> Option<(u64, Option<StoredResponse>)> {
    fn take<'a>(buf: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        (buf.len() >= n).then(|| {
            let (head, rest) = buf.split_at(n);
            *buf = rest;
            head
        })
    }
    fn u32_of(buf: &mut &[u8]) -> Option<usize> {
        Some(u32::from_be_bytes(take(buf, 4)?.try_into().ok()?) as usize)
    }
    let mut buf = raw;
    let fingerprint = u64::from_be_bytes(take(&mut buf, 8)?.try_into().ok()?);
    match take(&mut buf, 1)?[0] {
        RUNNING => Some((fingerprint, None)),
        DONE => {
            let status = u16::from_be_bytes(take(&mut buf, 2)?.try_into().ok()?);
            let count = u32_of(&mut buf)?;
            let mut headers = Vec::new();
            for _ in 0..count {
                let n = u32_of(&mut buf)?;
                let name = String::from_utf8(take(&mut buf, n)?.to_vec()).ok()?;
                let n = u32_of(&mut buf)?;
                headers.push((name, take(&mut buf, n)?.to_vec()));
            }
            Some((
                fingerprint,
                Some(StoredResponse {
                    status,
                    headers,
                    body: Bytes::copy_from_slice(buf),
                }),
            ))
        }
        _ => None,
    }
}

impl RedisIdempotencyStore {
    pub fn new(redis: Redis) -> Self {
        Self {
            redis,
            prefix: "ullun:idem:".into(),
        }
    }

    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    fn key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    async fn try_begin(&self, key: &str, fingerprint: u64, lock_ttl: Duration) -> Result<Begin> {
        let k = self.key(key);
        let claim = encode_entry(fingerprint, None);
        // a key can expire between the failed claim and the read: try again
        for _ in 0..3 {
            let set = Cmd::new("SET")
                .arg(&k)
                .arg(&claim)
                .arg("NX")
                .arg("PX")
                .int(millis(lock_ttl));
            if self.redis.run(set).await?.is_ok() {
                return Ok(Begin::Started);
            }
            let Some(raw) = self.redis.run(Cmd::new("GET").arg(&k)).await?.into_bytes() else {
                continue;
            };
            return Ok(match decode_entry(&raw) {
                Some((fp, _)) if fp != fingerprint => Begin::Mismatch,
                Some((_, None)) => Begin::InFlight,
                Some((_, Some(res))) => Begin::Replay(res),
                // unreadable entry: refuse rather than guess
                None => Begin::Unavailable,
            });
        }
        Ok(Begin::Unavailable)
    }
}

#[async_trait]
impl IdempotencyStore for RedisIdempotencyStore {
    async fn begin(&self, key: &str, fingerprint: u64, lock_ttl: Duration) -> Begin {
        self.try_begin(key, fingerprint, lock_ttl)
            .await
            .unwrap_or_else(|e| {
                eprintln!("Idempotency store unavailable: {}", e.message());
                Begin::Unavailable
            })
    }

    async fn complete(&self, key: &str, response: StoredResponse, ttl: Duration) {
        let k = self.key(key);
        // keep the fingerprint of the claim; XX: only a claim still ours is finished
        let Ok(Some(raw)) = self
            .redis
            .run(Cmd::new("GET").arg(&k))
            .await
            .map(Value::into_bytes)
        else {
            return;
        };
        let Some((fingerprint, _)) = decode_entry(&raw) else {
            return;
        };
        let cmd = Cmd::new("SET")
            .arg(&k)
            .arg(encode_entry(fingerprint, Some(&response)))
            .arg("XX")
            .arg("PX")
            .int(millis(ttl));
        if let Err(e) = self.redis.run(cmd).await {
            eprintln!("Saving idempotent response failed: {}", e.message());
        }
    }

    async fn abandon(&self, key: &str) {
        let k = self.key(key);
        // only a running claim is released, never a stored response
        if let Ok(Value::Bulk(raw)) = self.redis.run(Cmd::new("GET").arg(&k)).await {
            if matches!(decode_entry(&raw), Some((_, None))) {
                let _ = self.redis.run(Cmd::new("DEL").arg(&k)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_encoded_as_resp_arrays() {
        let cmd = Cmd::new("SET").arg("k").arg(b"v\r\n").int(5);
        assert_eq!(
            cmd.encode(),
            b"*4\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nv\r\n\r\n$1\r\n5\r\n".to_vec()
        );
    }

    #[test]
    fn urls_are_parsed() {
        let r = Redis::from_url("redis://localhost").unwrap();
        assert_eq!((r.addr.as_str(), r.db), ("localhost:6379", 0));
        let r = Redis::from_url("redis://h:7000/3").unwrap();
        assert_eq!((r.addr.as_str(), r.db), ("h:7000", 3));
        let r = Redis::from_url("redis://:p%40ss@h/").unwrap();
        assert_eq!(r.password.as_deref(), Some("p@ss"));
        assert_eq!(r.username, None);
        let r = Redis::from_url("redis://ann:secret@h:1/2").unwrap();
        assert_eq!(r.username.as_deref(), Some("ann"));
        assert_eq!(r.password.as_deref(), Some("secret"));
        for bad in [
            "",
            "localhost",
            "rediss://h",
            "redis://",
            "redis://h/x",
            "http://h",
        ] {
            assert!(Redis::from_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn idempotency_entries_roundtrip() {
        let running = encode_entry(42, None);
        assert_eq!(
            decode_entry(&running).map(|(f, r)| (f, r.is_none())),
            Some((42, true))
        );
        let res = StoredResponse {
            status: 201,
            headers: vec![
                ("content-type".into(), b"application/json".to_vec()),
                ("x".into(), vec![0, 255]),
            ],
            body: Bytes::from_static(b"{\"id\":1}\r\n\0"),
        };
        let (fp, got) = decode_entry(&encode_entry(7, Some(&res))).unwrap();
        let got = got.unwrap();
        assert_eq!(
            (fp, got.status, &got.headers, &got.body),
            (7, 201, &res.headers, &res.body)
        );
        // truncated or unknown entries are rejected
        let full = encode_entry(7, Some(&res));
        for cut in 0..full.len().min(40) {
            let _ = decode_entry(&full[..cut]);
        }
        assert!(decode_entry(&full[..12]).is_none());
        assert!(decode_entry(b"short").is_none());
        let mut unknown = encode_entry(1, None);
        *unknown.last_mut().unwrap() = 9;
        assert!(decode_entry(&unknown).is_none());
    }
}
