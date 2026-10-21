//! Liveness / readiness probes and the shutdown drain phase

mod common;

use common::{spawn, Server};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use ullun::health::Health;
use ullun::prelude::*;

fn app(health: &Health) -> App {
    App::new()
        .get("/livez", health.liveness())
        .get("/readyz", health.readiness())
        .get("/", |_req: Request| async { Ok(Response::text("hello")) })
}

#[tokio::test]
async fn liveness_never_runs_checks() {
    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    let health = Health::new().check("db", move || {
        r.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>("down") }
    });
    let s = spawn(app(&health)).await;
    let res = s.get("/livez").send().await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json()["status"], "up");
    assert_eq!(res.header("cache-control"), "no-store");
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn readiness_reports_each_check() {
    let health = Health::new()
        .check("db", || async { Ok::<(), String>(()) })
        .check("cache", || async { Ok::<(), String>(()) });
    let s = spawn(app(&health)).await;
    let res = s.get("/readyz").send().await;
    assert_eq!(res.status, 200);
    let body = res.json();
    assert_eq!(body["status"], "up");
    assert_eq!(body["checks"]["db"]["status"], "up");
    assert_eq!(body["checks"]["cache"]["critical"], true);
    assert!(body["checks"]["db"]["ms"].is_number());
}

#[tokio::test]
async fn failed_critical_check_is_503_and_hides_the_error() {
    let health = Health::new()
        .check("db", || async {
            Err::<(), _>("password auth failed for db-host-7")
        })
        .check("cache", || async { Ok::<(), String>(()) });
    let s = spawn(app(&health)).await;
    let res = s.get("/readyz").send().await;
    assert_eq!(res.status, 503);
    let body = res.json();
    assert_eq!(body["status"], "down");
    assert_eq!(body["checks"]["db"]["status"], "down");
    assert_eq!(body["checks"]["cache"]["status"], "up");
    assert!(body["checks"]["db"].get("error").is_none());
    assert!(!res.text().contains("db-host-7"));
}

#[tokio::test]
async fn errors_can_be_exposed_for_internal_endpoints() {
    let health = Health::new()
        .expose_errors()
        .check("db", || async { Err::<(), _>("connection refused") });
    let s = spawn(app(&health)).await;
    assert_eq!(
        s.get("/readyz").send().await.json()["checks"]["db"]["error"],
        "connection refused"
    );
}

#[tokio::test]
async fn optional_failures_only_degrade() {
    let health = Health::new()
        .check("db", || async { Ok::<(), String>(()) })
        .optional("recommendations", || async { Err::<(), _>("slow") });
    let s = spawn(app(&health)).await;
    let res = s.get("/readyz").send().await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json()["status"], "degraded");
}

#[tokio::test]
async fn slow_checks_time_out_and_run_concurrently() {
    let health = Health::new()
        .timeout(Duration::from_millis(100))
        .check("hang", || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<(), String>(())
        })
        .check("also-hang", || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<(), String>(())
        });
    let s = spawn(app(&health)).await;
    let started = std::time::Instant::now();
    let res = s.get("/readyz").send().await;
    assert_eq!(res.status, 503);
    // both timed out in parallel: well under 2 x timeout plus slack
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn results_are_cached_and_shared_between_concurrent_probes() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let health = Health::new()
        .cache_for(Duration::from_millis(300))
        .check("db", move || {
            r.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<(), String>(())
            }
        });
    let s = spawn(app(&health)).await;
    let probes: Vec<_> = (0..6)
        .map(|_| tokio::spawn(async move { s.get("/readyz").send().await.status }))
        .collect();
    for p in probes {
        assert_eq!(p.await.unwrap(), 200);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    tokio::time::sleep(Duration::from_millis(400)).await;
    s.get("/readyz").send().await;
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn checks_see_changing_state() {
    let up = Arc::new(AtomicBool::new(true));
    let u = up.clone();
    let health = Health::new()
        .cache_for(Duration::ZERO)
        .check("flag", move || {
            let ok = u.load(Ordering::SeqCst);
            async move {
                if ok {
                    Ok(())
                } else {
                    Err("off")
                }
            }
        });
    let s = spawn(app(&health)).await;
    assert_eq!(s.get("/readyz").send().await.status, 200);
    up.store(false, Ordering::SeqCst);
    assert_eq!(s.get("/readyz").send().await.status, 503);
}

#[tokio::test]
async fn readiness_drops_before_connections_are_refused() {
    let health = Health::new().check("db", || async { Ok::<(), String>(()) });
    let app = app(&health).shutdown_delay(Duration::from_millis(600));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(app.serve_with_shutdown(listener, async {
        let _ = stopped.await;
    }));
    let s = Server { addr };
    assert_eq!(s.get("/readyz").send().await.status, 200);

    stop.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // draining: still serving, but not ready
    let ready = s.get("/readyz").send().await;
    assert_eq!(ready.status, 503);
    assert_eq!(ready.json()["status"], "draining");
    assert_eq!(s.get("/").send().await.status, 200);
    assert_eq!(s.get("/livez").send().await.status, 200);

    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("server did not stop after the delay")
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}
