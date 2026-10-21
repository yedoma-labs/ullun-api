//! Liveness and readiness endpoints
//!
//! Orchestrators ask two different questions. *Liveness*: is the process
//! stuck and should it be restarted? *Readiness*: should traffic be sent here
//! right now? [`Health`] answers both:
//!
//! * [`Health::liveness`] only proves the server answers requests; it never
//!   touches dependencies, so a database outage does not restart every instance
//! * [`Health::readiness`] runs the registered checks (each with a timeout,
//!   results cached briefly so probes cannot hammer the database) and answers
//!   `503` when a critical one fails. Optional checks only mark the service as
//!   `degraded`. During a graceful shutdown it answers `503` immediately: with
//!   [`App::shutdown_delay`](crate::app::App::shutdown_delay) the load balancer
//!   stops routing to the instance before connections are refused.
//!
//! ```no_run
//! use std::time::Duration;
//! use ullun::health::Health;
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! let health = Health::new()
//!     .check("database", || async { Ok::<(), String>(()) })
//!     .optional("recommendations", || async { Err::<(), _>("slow upstream") });
//! App::new()
//!     .shutdown_delay(Duration::from_secs(5))
//!     .get("/livez", health.liveness())
//!     .get("/readyz", health.readiness())
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! Error messages of failed checks are not included in the response (it is
//! usually public); call [`Health::expose_errors`] for internal endpoints.

use crate::error::Result;
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::Response;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

type CheckFn = dyn Fn() -> BoxFuture<'static, std::result::Result<(), String>> + Send + Sync;

struct Check {
    name: String,
    critical: bool,
    run: Arc<CheckFn>,
}

#[derive(Clone)]
struct Outcome {
    ok: bool,
    critical: bool,
    millis: u64,
    error: Option<String>,
}

struct Cached {
    at: Instant,
    outcomes: Arc<BTreeMap<String, Outcome>>,
}

struct Inner {
    checks: Vec<Check>,
    timeout: Duration,
    cache_for: Duration,
    expose_errors: bool,
    cache: Mutex<Option<Cached>>,
}

/// Health check registry, see the [module documentation](self). Cloning gives
/// another handle to the same registry.
#[derive(Clone)]
pub struct Health {
    inner: Arc<Inner>,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                checks: Vec::new(),
                timeout: Duration::from_secs(2),
                cache_for: Duration::from_secs(1),
                expose_errors: false,
                cache: Mutex::new(None),
            }),
        }
    }

    fn edit(mut self, f: impl FnOnce(&mut Inner)) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("configure Health before sharing it");
        f(inner);
        self
    }

    fn add<F, Fut, E>(self, name: &str, critical: bool, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: Display,
    {
        let name = name.to_string();
        self.edit(|i| {
            i.checks.push(Check {
                name,
                critical,
                run: Arc::new(move || {
                    let fut = check();
                    Box::pin(async move { fut.await.map_err(|e| e.to_string()) })
                }),
            })
        })
    }

    /// A dependency the service cannot work without: when it fails, readiness is `503`
    pub fn check<F, Fut, E>(self, name: &str, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: Display,
    {
        self.add(name, true, check)
    }

    /// A dependency the service can live without: when it fails, readiness stays
    /// `200` but reports `degraded`
    pub fn optional<F, Fut, E>(self, name: &str, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: Display,
    {
        self.add(name, false, check)
    }

    /// Longest a single check may take before it counts as failed (default 2 seconds)
    pub fn timeout(self, timeout: Duration) -> Self {
        self.edit(|i| i.timeout = timeout)
    }

    /// Reuse the last result for this long (default 1 second, zero disables).
    /// Concurrent probes share one run of the checks either way.
    pub fn cache_for(self, ttl: Duration) -> Self {
        self.edit(|i| i.cache_for = ttl)
    }

    /// Include the error text of failed checks in the response. Off by default
    /// because messages can reveal hostnames and internals.
    pub fn expose_errors(self) -> Self {
        self.edit(|i| i.expose_errors = true)
    }

    /// Handler for liveness probes: `200 {"status":"up"}` as long as the server responds
    pub fn liveness(
        &self,
    ) -> impl Fn(Request) -> BoxFuture<'static, Result<Response>> + Send + Sync + Clone + 'static
    {
        |_req| {
            Box::pin(async {
                Ok(Response::json(serde_json::json!({ "status": "up" }))
                    .with_header("cache-control", "no-store"))
            })
        }
    }

    /// Handler for readiness probes, see the [module documentation](self)
    pub fn readiness(
        &self,
    ) -> impl Fn(Request) -> BoxFuture<'static, Result<Response>> + Send + Sync + Clone + 'static
    {
        let health = self.clone();
        move |req: Request| {
            let health = health.clone();
            Box::pin(async move { Ok(health.report(&req).await) })
        }
    }

    async fn outcomes(&self) -> Arc<BTreeMap<String, Outcome>> {
        let inner = &self.inner;
        // holding the lock while checks run makes concurrent probes wait for one run
        let mut cache = inner.cache.lock().await;
        if let Some(c) = cache.as_ref() {
            if c.at.elapsed() < inner.cache_for {
                return c.outcomes.clone();
            }
        }
        let runs = inner.checks.iter().map(|check| async move {
            let started = Instant::now();
            let result = tokio::time::timeout(inner.timeout, (check.run)()).await;
            let (ok, error) = match result {
                Ok(Ok(())) => (true, None),
                Ok(Err(e)) => (false, Some(e)),
                Err(_) => (false, Some(format!("timed out after {:?}", inner.timeout))),
            };
            (
                check.name.clone(),
                Outcome {
                    ok,
                    critical: check.critical,
                    millis: started.elapsed().as_millis() as u64,
                    error,
                },
            )
        });
        let outcomes: BTreeMap<_, _> = join_all(runs).await.into_iter().collect();
        let outcomes = Arc::new(outcomes);
        *cache = Some(Cached {
            at: Instant::now(),
            outcomes: outcomes.clone(),
        });
        outcomes
    }

    async fn report(&self, req: &Request) -> Response {
        let no_store = |r: Response| r.with_header("cache-control", "no-store");
        if req.shutdown_signal().is_draining() {
            return no_store(
                Response::json(serde_json::json!({ "status": "draining" }))
                    .with_status(http::StatusCode::SERVICE_UNAVAILABLE),
            );
        }
        let outcomes = self.outcomes().await;
        let down = outcomes.values().any(|o| !o.ok && o.critical);
        let degraded = outcomes.values().any(|o| !o.ok && !o.critical);
        let status = if down {
            "down"
        } else if degraded {
            "degraded"
        } else {
            "up"
        };
        let checks: serde_json::Map<_, _> = outcomes
            .iter()
            .map(|(name, o)| {
                let mut entry = serde_json::json!({
                    "status": if o.ok { "up" } else { "down" },
                    "critical": o.critical,
                    "ms": o.millis,
                });
                if let (false, Some(err), true) = (o.ok, &o.error, self.inner.expose_errors) {
                    entry["error"] = err.clone().into();
                }
                (name.clone(), entry)
            })
            .collect();
        let code = if down {
            http::StatusCode::SERVICE_UNAVAILABLE
        } else {
            http::StatusCode::OK
        };
        no_store(
            Response::json(serde_json::json!({ "status": status, "checks": checks }))
                .with_status(code),
        )
    }
}

/// Run futures concurrently and collect their outputs in order
async fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut futures: Vec<_> = futures.into_iter().map(Box::pin).collect();
    let mut done: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in futures.iter_mut().enumerate() {
            if done[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => done[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    done.into_iter().flatten().collect()
}
