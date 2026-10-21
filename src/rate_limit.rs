//! In-memory rate limiting middleware
//!
//! Fixed window per key (the peer IP by default), or a sliding window with
//! [`RateLimiter::sliding_window`]. State lives in this process,
//! so with several instances each one enforces the limit on its own; put a
//! shared limiter (reverse proxy, Redis) in front for a global limit.
//!
//! ```no_run
//! use std::time::Duration;
//! use ullun::prelude::*;
//! use ullun::rate_limit::RateLimiter;
//!
//! # async fn run() {
//! App::new()
//!     .middleware(RateLimiter::new(100, Duration::from_secs(60)))
//!     .get("/", |_req: Request| async { Ok(Response::text("ok")) })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```

use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type KeyFn = dyn Fn(&Request) -> Option<String> + Send + Sync;

struct Window {
    started: Instant,
    count: u32,
    /// Requests of the previous window (sliding mode)
    prev: u32,
}

/// Outcome of counting one request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    /// Seconds until the current window ends (at least 1)
    pub reset_secs: u64,
}

/// Fixed-window rate limiter, usable as middleware
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    max_keys: usize,
    sliding: bool,
    key_fn: Arc<KeyFn>,
    state: Mutex<HashMap<String, Window>>,
}

impl RateLimiter {
    /// Allow `limit` requests per `window` for each client IP
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            max_keys: 100_000,
            sliding: false,
            key_fn: Arc::new(|req| req.client_ip().map(|ip| ip.to_string())),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Count over a window that slides with time instead of resetting at fixed
    /// boundaries. A fixed window lets a client send `limit` requests just before
    /// the reset and `limit` more just after (twice the limit within a moment);
    /// the sliding window weighs the previous window by how much of it still
    /// overlaps, so the rate stays at `limit` per window at any moment.
    pub fn sliding_window(mut self) -> Self {
        self.sliding = true;
        self
    }

    /// Choose the bucket per request, e.g. an API key header. Returning `None`
    /// exempts the request from limiting.
    pub fn key_by<F>(mut self, f: F) -> Self
    where
        F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
    {
        self.key_fn = Arc::new(f);
        self
    }

    /// Use the first `X-Forwarded-For` address, falling back to the peer IP.
    ///
    /// Only enable this behind a reverse proxy that sets the header: otherwise
    /// clients choose their own bucket and can evade the limit. Prefer
    /// [`App::trusted_proxies`](crate::app::App::trusted_proxies): the default
    /// key then is the real client address, found by skipping your own proxies
    /// from the right, which clients cannot influence.
    pub fn trust_forwarded_for(self) -> Self {
        self.key_by(|req| {
            req.headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .map(|ip| ip.trim().to_string())
                .filter(|ip| !ip.is_empty())
                .or_else(|| req.remote_addr.map(|a| a.ip().to_string()))
        })
    }

    /// Cap on tracked keys (default 100 000). When full after dropping expired
    /// windows, unseen keys are let through untracked rather than blocked.
    pub fn max_keys(mut self, max_keys: usize) -> Self {
        self.max_keys = max_keys;
        self
    }

    /// Count one request for `key` at time `now`
    pub fn check(&self, key: &str, now: Instant) -> Decision {
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());

        if map.len() >= self.max_keys && !map.contains_key(key) {
            let window = self.window;
            map.retain(|_, w| now.duration_since(w.started) < window);
            if map.len() >= self.max_keys {
                return Decision {
                    allowed: true,
                    limit: self.limit,
                    remaining: self.limit,
                    reset_secs: self.window.as_secs().max(1),
                };
            }
        }

        let w = map.entry(key.to_string()).or_insert(Window {
            started: now,
            count: 0,
            prev: 0,
        });
        if self.sliding {
            return self.check_sliding(w, now);
        }
        if now.duration_since(w.started) >= self.window {
            *w = Window {
                started: now,
                count: 0,
                prev: 0,
            };
        }
        let elapsed = now.duration_since(w.started);
        let reset_secs = self
            .window
            .saturating_sub(elapsed)
            .as_secs_f64()
            .ceil()
            .max(1.0) as u64;

        if w.count < self.limit {
            w.count += 1;
            Decision {
                allowed: true,
                limit: self.limit,
                remaining: self.limit - w.count,
                reset_secs,
            }
        } else {
            Decision {
                allowed: false,
                limit: self.limit,
                remaining: 0,
                reset_secs,
            }
        }
    }

    /// Sliding-window counter: requests of the previous window count in
    /// proportion to how much of it the last `window` still covers
    fn check_sliding(&self, w: &mut Window, now: Instant) -> Decision {
        let win = self.window.as_secs_f64().max(f64::EPSILON);
        let mut elapsed = now.duration_since(w.started).as_secs_f64();
        if elapsed >= win {
            let periods = (elapsed / win).floor();
            w.prev = if periods < 2.0 { w.count } else { 0 };
            w.count = 0;
            w.started += Duration::from_secs_f64(periods * win);
            elapsed = now.duration_since(w.started).as_secs_f64();
        }
        let weight = 1.0 - elapsed / win;
        let estimate = f64::from(w.prev) * weight + f64::from(w.count);
        let limit = f64::from(self.limit);

        if estimate + 1.0 <= limit {
            w.count += 1;
            let used = estimate + 1.0;
            return Decision {
                allowed: true,
                limit: self.limit,
                remaining: (limit - used).floor().max(0.0) as u32,
                reset_secs: (win - elapsed).ceil().max(1.0) as u64,
            };
        }
        // when does the estimate drop far enough for one more request?
        let wait = if w.prev > 0 && f64::from(w.count) < limit {
            // the previous window keeps decaying: solve prev * (1 - t / win) + count <= limit - 1
            let need = estimate + 1.0 - limit;
            (need * win / f64::from(w.prev)).min(win - elapsed)
        } else {
            win - elapsed
        };
        Decision {
            allowed: false,
            limit: self.limit,
            remaining: 0,
            reset_secs: wait.ceil().max(1.0) as u64,
        }
    }
}

#[async_trait]
impl Middleware for RateLimiter {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        // CORS preflights carry no credentials and must not eat the budget
        if req.method == http::Method::OPTIONS {
            return next.run(req).await;
        }
        let Some(key) = (self.key_fn)(&req) else {
            return next.run(req).await;
        };

        let decision = self.check(&key, Instant::now());
        let res = if decision.allowed {
            match next.run(req).await {
                Ok(res) => res,
                Err(err) => Response::from_error(err),
            }
        } else {
            Response::from_error(Error::custom(429, "Too many requests"))
                .with_header("retry-after", decision.reset_secs.to_string())
        };

        Ok(res
            .with_header("x-ratelimit-limit", decision.limit.to_string())
            .with_header("x-ratelimit-remaining", decision.remaining.to_string())
            .with_header("x-ratelimit-reset", decision.reset_secs.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_within_window_and_resets_after() {
        let rl = RateLimiter::new(2, Duration::from_secs(10));
        let t0 = Instant::now();
        let d1 = rl.check("a", t0);
        assert_eq!((d1.allowed, d1.remaining), (true, 1));
        assert_eq!(rl.check("a", t0 + Duration::from_secs(1)).remaining, 0);

        let blocked = rl.check("a", t0 + Duration::from_millis(2500));
        assert!(!blocked.allowed);
        assert_eq!((blocked.remaining, blocked.reset_secs), (0, 8));

        let again = rl.check("a", t0 + Duration::from_secs(10));
        assert!(again.allowed && again.remaining == 1);
    }

    #[test]
    fn keys_are_independent() {
        let rl = RateLimiter::new(1, Duration::from_secs(60));
        let t = Instant::now();
        assert!(rl.check("a", t).allowed);
        assert!(!rl.check("a", t).allowed);
        assert!(rl.check("b", t).allowed);
    }

    #[test]
    fn full_table_drops_expired_then_fails_open() {
        let rl = RateLimiter::new(1, Duration::from_secs(10)).max_keys(2);
        let t = Instant::now();
        rl.check("a", t);
        rl.check("b", t);
        // table full and nothing expired: untracked, allowed, not remembered
        assert!(rl.check("c", t).allowed);
        assert!(rl.check("c", t).allowed);
        assert!(!rl.check("a", t).allowed);
        // later the expired entries are swept and "c" gets tracked
        let later = t + Duration::from_secs(11);
        assert!(rl.check("c", later).allowed);
        assert!(!rl.check("c", later).allowed);
    }

    #[test]
    fn sliding_window_has_no_boundary_burst() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        // fixed window: 10 just before the reset and 10 just after all pass
        let fixed = RateLimiter::new(10, secs(10));
        assert!(fixed.check("a", t0).allowed);
        for _ in 0..9 {
            assert!(fixed.check("a", at(9.0)).allowed);
        }
        let burst = (0..10)
            .filter(|_| fixed.check("a", at(10.5)).allowed)
            .count();
        assert_eq!(burst, 10);

        // sliding: the same pattern is held to the limit
        let sliding = RateLimiter::new(10, secs(10)).sliding_window();
        assert!(sliding.check("a", t0).allowed);
        for _ in 0..9 {
            assert!(sliding.check("a", at(9.0)).allowed);
        }
        let allowed = (0..10)
            .filter(|_| sliding.check("a", at(10.5)).allowed)
            .count();
        assert_eq!(allowed, 0);
    }

    #[test]
    fn sliding_window_recovers_gradually_and_reports_when() {
        let rl = RateLimiter::new(10, Duration::from_secs(10)).sliding_window();
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(rl.check("k", t0).allowed);
        }
        let blocked = rl.check("k", t0 + Duration::from_secs(1));
        assert!(!blocked.allowed);
        assert!(blocked.reset_secs >= 1 && blocked.reset_secs <= 10);

        // after a full window nothing is left over
        let later = rl.check("k", t0 + Duration::from_secs(25));
        assert!(later.allowed);
        assert_eq!(later.remaining, 9);

        // half a window later about half of the old requests no longer count
        let rl = RateLimiter::new(10, Duration::from_secs(10)).sliding_window();
        for _ in 0..10 {
            rl.check("k", t0);
        }
        let allowed = (0..10)
            .filter(|_| rl.check("k", t0 + Duration::from_secs(15)).allowed)
            .count();
        assert_eq!(allowed, 5);
    }

    #[test]
    fn sliding_remaining_counts_down() {
        let rl = RateLimiter::new(3, Duration::from_secs(60)).sliding_window();
        let t = Instant::now();
        assert_eq!(rl.check("a", t).remaining, 2);
        assert_eq!(rl.check("a", t).remaining, 1);
        assert_eq!(rl.check("a", t).remaining, 0);
        assert!(!rl.check("a", t).allowed);
    }

    #[test]
    fn zero_limit_blocks_everything() {
        let rl = RateLimiter::new(0, Duration::from_secs(1));
        assert!(!rl.check("a", Instant::now()).allowed);
    }
}
