//! Scenario: containerised microservice (Kubernetes / Docker)
//!
//! - liveness (`/healthz`) and readiness (`/readyz`) probes
//! - request-id propagation
//! - Prometheus text metrics (`/metrics`) from lock-free counters
//! - configuration via environment (`PORT`, default 3000)
//! - graceful shutdown on CTRL+C / SIGINT is handled by `App::run`
//!
//! Run:  cargo run --example microservice
//! Try:  curl -i localhost:3000/orders/42
//!       curl localhost:3000/metrics

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use ullun::prelude::*;

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    errors: AtomicU64,
    in_flight: AtomicU64,
}

#[tokio::main]
async fn main() {
    let metrics = Arc::new(Metrics::default());
    let next_id = Arc::new(AtomicU64::new(1));

    // request-id: reuse the caller's id or mint one, echo it on the response
    let request_id = move |req: Request, next: Next| {
        let next_id = next_id.clone();
        async move {
            let id = req
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty() && v.len() <= 64 && v.is_ascii())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("req-{:08x}", next_id.fetch_add(1, Ordering::Relaxed)));
            let mut res = match next.run(req).await {
                Ok(res) => res,
                Err(err) => Response::from_error(err),
            };
            res = res.with_header("x-request-id", id);
            Ok(res)
        }
    };

    // metrics: count requests, 5xx/4xx responses and in-flight gauge
    let m = metrics.clone();
    let measure = move |req: Request, next: Next| {
        let m = m.clone();
        async move {
            m.requests.fetch_add(1, Ordering::Relaxed);
            m.in_flight.fetch_add(1, Ordering::Relaxed);
            let result = next.run(req).await;
            m.in_flight.fetch_sub(1, Ordering::Relaxed);
            let failed = match &result {
                Ok(res) => res.status.as_u16() >= 400,
                Err(_) => true,
            };
            if failed {
                m.errors.fetch_add(1, Ordering::Relaxed);
            }
            result
        }
    };

    let m = metrics.clone();
    let metrics_endpoint = move |_req: Request| {
        let m = m.clone();
        async move {
            let body = format!(
                "# TYPE http_requests_total counter\nhttp_requests_total {}\n\
                 # TYPE http_errors_total counter\nhttp_errors_total {}\n\
                 # TYPE http_requests_in_flight gauge\nhttp_requests_in_flight {}\n",
                m.requests.load(Ordering::Relaxed),
                m.errors.load(Ordering::Relaxed),
                m.in_flight.load(Ordering::Relaxed),
            );
            Ok(Response::text(body)
                .with_header("content-type", "text/plain; version=0.0.4; charset=utf-8"))
        }
    };

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());

    App::new()
        .middleware(request_id)
        .middleware(measure)
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(serde_json::json!({ "status": "alive" })))
        })
        .get("/readyz", |_req: Request| async {
            // check real dependencies here (database, queue, ...)
            Ok(Response::json(serde_json::json!({ "status": "ready" })))
        })
        .get("/metrics", metrics_endpoint)
        .get_with_params("/orders/{id}", |params: Params| async move {
            let id: u64 = params
                .get("id")?
                .parse()
                .map_err(|_| Error::bad_request("order id must be a number"))?;
            if id == 0 {
                return Err(Error::not_found("order not found"));
            }
            Ok(Response::json(
                serde_json::json!({ "id": id, "status": "shipped" }),
            ))
        })
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
