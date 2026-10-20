//! Resilience limits: request timeouts, load shedding and connection caps
//!
//! * [`Timeout`] answers 504 when a handler takes too long
//! * [`ConcurrencyLimit`] sheds load with 503 + `Retry-After` instead of letting
//!   work pile up until the process falls over
//! * [`App::max_connections`](crate::app::App::max_connections),
//!   [`App::max_connections_per_ip`](crate::app::App::max_connections_per_ip) and
//!   [`App::header_read_timeout`](crate::app::App::header_read_timeout) bound what
//!   a single client (or a slowloris) can hold on to
//!
//! ```no_run
//! use std::time::Duration;
//! use ullun::limits::{ConcurrencyLimit, Timeout};
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! App::new()
//!     .header_read_timeout(Duration::from_secs(10))
//!     .max_connections(10_000)
//!     .max_connections_per_ip(200)
//!     .middleware(Timeout::new(Duration::from_secs(15)))
//!     .middleware(ConcurrencyLimit::new(512))
//!     .get("/", |_req: Request| async { Ok(Response::text("ok")) })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Both middlewares cover the time until the handler returns its [`Response`];
//! a streaming body (SSE, downloads) is produced afterwards and is not cut off.

use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Fail requests that run longer than a deadline with 504
pub struct Timeout {
    limit: Duration,
}

impl Timeout {
    pub fn new(limit: Duration) -> Self {
        Self { limit }
    }
}

#[async_trait]
impl Middleware for Timeout {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        match tokio::time::timeout(self.limit, next.run(req)).await {
            Ok(result) => result,
            Err(_) => Ok(Response::from_error(Error::custom(
                504,
                "Request timed out",
            ))),
        }
    }
}

/// Allow at most `max` requests in flight, answer the rest with 503 and `Retry-After`
///
/// By default an excess request is rejected at once. [`ConcurrencyLimit::queue_for`]
/// lets it wait a short while for a free slot first, which smooths bursts without
/// building an unbounded backlog.
pub struct ConcurrencyLimit {
    permits: Arc<Semaphore>,
    wait: Duration,
    retry_after: u64,
}

impl ConcurrencyLimit {
    pub fn new(max: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max.max(1))),
            wait: Duration::ZERO,
            retry_after: 1,
        }
    }

    /// Let an excess request wait up to `wait` for a slot before it is rejected
    pub fn queue_for(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// Seconds advertised in `Retry-After` (default 1)
    pub fn retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = seconds;
        self
    }

    /// Requests that currently hold a slot is `max - available_slots()`
    pub fn available_slots(&self) -> usize {
        self.permits.available_permits()
    }
}

#[async_trait]
impl Middleware for ConcurrencyLimit {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) if self.wait.is_zero() => None,
            Err(_) => tokio::time::timeout(self.wait, self.permits.clone().acquire_owned())
                .await
                .ok()
                .and_then(|r| r.ok()),
        };
        let Some(_permit) = permit else {
            return Ok(Response::from_error(Error::custom(
                503,
                "Server is busy, try again shortly",
            ))
            .with_header("retry-after", self.retry_after.to_string()));
        };
        next.run(req).await
    }
}

type PerIp = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// Connection caps checked on accept
pub(crate) struct ConnLimits {
    total: Option<Arc<Semaphore>>,
    per_ip: Option<(usize, PerIp)>,
}

/// Releases a connection's slot when dropped
pub(crate) struct ConnGuard {
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ip: Option<(IpAddr, PerIp)>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some((ip, map)) = self.ip.take() {
            let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = map.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    map.remove(&ip);
                }
            }
        }
    }
}

impl ConnLimits {
    pub(crate) fn new(total: Option<usize>, per_ip: Option<usize>) -> Self {
        Self {
            total: total.map(|n| Arc::new(Semaphore::new(n))),
            per_ip: per_ip.map(|n| (n, Arc::default())),
        }
    }

    /// A slot for a new connection from `ip`, or `None` when a cap is reached
    pub(crate) fn admit(&self, ip: IpAddr) -> Option<ConnGuard> {
        let permit = match &self.total {
            Some(sem) => Some(sem.clone().try_acquire_owned().ok()?),
            None => None,
        };
        let ip = match &self.per_ip {
            Some((max, map)) => {
                let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
                let n = guard.entry(ip).or_insert(0);
                if *n >= *max {
                    return None; // dropping `permit` frees the total slot again
                }
                *n += 1;
                Some((ip, map.clone()))
            }
            None => None,
        };
        Some(ConnGuard {
            _permit: permit,
            ip,
        })
    }
}

/// Tell a refused client to come back later, without reading its request
pub(crate) async fn refuse<S>(mut stream: S)
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let reply = b"HTTP/1.1 503 Service Unavailable\r\nretry-after: 1\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        let _ = stream.write_all(reply).await;
        let _ = stream.shutdown().await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn total_cap_releases_on_drop() {
        let limits = ConnLimits::new(Some(2), None);
        let a = limits.admit(ip(1)).unwrap();
        let _b = limits.admit(ip(2)).unwrap();
        assert!(limits.admit(ip(3)).is_none());
        drop(a);
        assert!(limits.admit(ip(3)).is_some());
    }

    #[test]
    fn per_ip_cap_is_independent_per_address() {
        let limits = ConnLimits::new(None, Some(1));
        let a = limits.admit(ip(1)).unwrap();
        assert!(limits.admit(ip(1)).is_none());
        assert!(limits.admit(ip(2)).is_some());
        drop(a);
        assert!(limits.admit(ip(1)).is_some());
    }

    #[test]
    fn refused_per_ip_does_not_leak_a_total_slot() {
        let limits = ConnLimits::new(Some(2), Some(1));
        let _a = limits.admit(ip(1)).unwrap();
        assert!(limits.admit(ip(1)).is_none());
        assert!(limits.admit(ip(2)).is_some());
    }

    #[test]
    fn no_caps_admit_everything() {
        let limits = ConnLimits::new(None, None);
        let _g: Vec<_> = (0..100).map(|_| limits.admit(ip(1)).unwrap()).collect();
    }
}
