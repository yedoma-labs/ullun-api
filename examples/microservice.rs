//! Scenario: containerised microservice (Kubernetes / Docker)
//!
//! - liveness (`/healthz`) and readiness (`/readyz`) probes
//! - request ids (propagated from callers), JSON access log on stdout
//! - Prometheus metrics (`/metrics`) labelled by route pattern
//! - configuration via environment (`PORT`, default 3000)
//! - graceful shutdown on SIGTERM / CTRL+C is handled by `App::run`
//!
//! Run:  cargo run --example microservice
//! Try:  curl -i localhost:3000/orders/42
//!       curl localhost:3000/metrics

use ullun::observability::{AccessLog, Metrics, RequestIdLayer};
use ullun::prelude::*;

#[tokio::main]
async fn main() {
    let metrics = Metrics::new();
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());

    App::new()
        .middleware(RequestIdLayer::new())
        .middleware(AccessLog::new().json())
        .middleware(metrics.clone())
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(serde_json::json!({ "status": "alive" })))
        })
        .get("/readyz", |_req: Request| async {
            // check real dependencies here (database, queue, ...)
            Ok(Response::json(serde_json::json!({ "status": "ready" })))
        })
        // keep this off the public internet: scrape it from inside the cluster
        .get("/metrics", metrics.handler())
        .get_with_params("/orders/{id}", |params: Params| async move {
            let id: u64 = params.parse("id")?;
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
