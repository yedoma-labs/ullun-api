//! Response cache over real HTTP

mod common;

use common::{spawn, Server};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ullun::cache::ResponseCache;
use ullun::prelude::*;

struct Calls(Arc<AtomicUsize>);

fn app(cache: ResponseCache) -> (App, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = App::new()
        .state(Calls(calls.clone()))
        .middleware(cache)
        .get("/report", |req: Request| async move {
            let n = req.state::<Calls>()?.0.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(Response::text(format!("run {n}")))
        })
        .get("/lang", |req: Request| async move {
            let lang = req
                .headers
                .get("accept-language")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("en")
                .to_string();
            req.state::<Calls>()?.0.fetch_add(1, Ordering::SeqCst);
            Ok(Response::text(lang))
        })
        .get("/private", |req: Request| async move {
            req.state::<Calls>()?.0.fetch_add(1, Ordering::SeqCst);
            Ok(Response::text("x").with_header("cache-control", "private"))
        })
        .get("/cookie", |req: Request| async move {
            req.state::<Calls>()?.0.fetch_add(1, Ordering::SeqCst);
            Ok(Response::text("x").cookie(Cookie::new("a", "b")))
        })
        .get("/fail", |req: Request| async move {
            let n = req.state::<Calls>()?.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            if n == 0 {
                Err(Error::internal("boom"))
            } else {
                Ok(Response::text("ok"))
            }
        })
        .get("/short", |req: Request| async move {
            req.state::<Calls>()?.0.fetch_add(1, Ordering::SeqCst);
            Ok(Response::text("x").with_header("cache-control", "max-age=1"))
        })
        .post("/report", |_req: Request| async {
            Ok(Response::text("posted"))
        });
    (app, calls)
}

async fn get(s: &Server, path: &str) -> (u16, String, String) {
    let r = s.get(path).send().await;
    (r.status, r.header("x-cache").to_string(), r.text())
}

#[tokio::test]
async fn second_request_is_a_hit_with_age() {
    let (app, calls) = app(ResponseCache::new());
    let s = spawn(app).await;
    let first = get(&s, "/report").await;
    assert_eq!(
        (first.0, first.1.as_str(), first.2.as_str()),
        (200, "MISS", "run 1")
    );
    let second = s.get("/report").send().await;
    assert_eq!(second.header("x-cache"), "HIT");
    assert_eq!(second.text(), "run 1");
    assert!(second.headers.contains_key("age"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_misses_run_the_handler_once() {
    let (app, calls) = app(ResponseCache::new());
    let s = spawn(app).await;
    let tasks: Vec<_> = (0..8)
        .map(|_| tokio::spawn(async move { get(&s, "/report").await }))
        .collect();
    let mut states = Vec::new();
    for t in tasks {
        let (status, state, body) = t.await.unwrap();
        assert_eq!((status, body.as_str()), (200, "run 1"));
        states.push(state);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(states.iter().filter(|s| *s == "MISS").count(), 1);
    assert!(states
        .iter()
        .all(|s| s == "MISS" || s == "COALESCED" || s == "HIT"));
}

#[tokio::test]
async fn entries_expire() {
    let (app, calls) = app(ResponseCache::new().ttl(Duration::from_millis(200)));
    let s = spawn(app).await;
    get(&s, "/report").await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    let again = get(&s, "/report").await;
    assert_eq!((again.1.as_str(), again.2.as_str()), ("MISS", "run 2"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn stale_responses_are_served_while_one_refresh_runs() {
    let (app, calls) = app(ResponseCache::new()
        .ttl(Duration::from_secs(1))
        .stale_while_revalidate(Duration::from_secs(30)));
    let s = spawn(app).await;
    get(&s, "/report").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;

    let stale = get(&s, "/report").await;
    let also_stale = get(&s, "/report").await;
    assert_eq!((stale.1.as_str(), stale.2.as_str()), ("STALE", "run 1"));
    assert_eq!(also_stale.1, "STALE");
    // both got the old body at once: neither waited for the refresh

    tokio::time::sleep(Duration::from_millis(400)).await;
    let fresh = get(&s, "/report").await;
    assert_eq!((fresh.1.as_str(), fresh.2.as_str()), ("HIT", "run 2"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn query_and_vary_headers_make_separate_entries() {
    let (app, calls) = app(ResponseCache::new().vary_by(["Accept-Language"]));
    let s = spawn(app).await;
    for (q, lang) in [("/lang", "de"), ("/lang", "fr"), ("/lang?x=1", "de")] {
        let r = s.get(q).header("accept-language", lang).send().await;
        assert_eq!(r.text(), lang);
        assert_eq!(r.header("x-cache"), "MISS");
        assert!(r.header("vary").contains("accept-language"));
    }
    let hit = s.get("/lang").header("accept-language", "de").send().await;
    assert_eq!(
        (hit.header("x-cache"), hit.text()),
        ("HIT", "de".to_string())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn credentialed_requests_bypass_unless_per_user() {
    let (app, calls) = app(ResponseCache::new());
    let s = spawn(app).await;
    s.get("/report").bearer("a").send().await;
    let second = s.get("/report").bearer("a").send().await;
    assert!(!second.headers.contains_key("x-cache"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let (app, calls) = app_per_user();
    let s = spawn(app).await;
    for token in ["a", "a", "b"] {
        s.get("/report").bearer(token).send().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

fn app_per_user() -> (App, Arc<AtomicUsize>) {
    app(ResponseCache::new().per_user())
}

#[tokio::test]
async fn unsafe_to_store_responses_are_not_stored() {
    let (app, calls) = app(ResponseCache::new());
    let s = spawn(app).await;
    for path in ["/private", "/cookie"] {
        get(&s, path).await;
        let again = get(&s, path).await;
        assert_eq!(again.1, "BYPASS", "{path}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn errors_are_not_cached_and_waiters_retry_themselves() {
    let (app, _) = app(ResponseCache::new());
    let s = spawn(app).await;
    let a = tokio::spawn(async move { get(&s, "/fail").await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let b = tokio::spawn(async move { get(&s, "/fail").await });
    assert_eq!(a.await.unwrap().0, 500);
    // the waiter ran the handler itself after the leader failed
    assert_eq!(b.await.unwrap().0, 200);
    assert_eq!(get(&s, "/fail").await.1, "HIT");
}

#[tokio::test]
async fn cache_control_max_age_overrides_the_default_ttl() {
    let (app, calls) = app(ResponseCache::new().ttl(Duration::from_secs(3600)));
    let s = spawn(app).await;
    get(&s, "/short").await;
    assert_eq!(get(&s, "/short").await.1, "HIT");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(get(&s, "/short").await.1, "MISS");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn non_get_methods_pass_through_and_invalidation_works() {
    let cache = ResponseCache::new();
    let (app, calls) = app(cache.clone());
    let s = spawn(app).await;
    get(&s, "/report").await;
    get(&s, "/lang").await;
    assert_eq!(cache.len(), 2);

    let post = s.post("/report").send().await;
    assert!(!post.headers.contains_key("x-cache"));

    cache.invalidate_prefix("/report");
    assert_eq!(cache.len(), 1);
    assert_eq!(get(&s, "/report").await.1, "MISS");
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    cache.clear();
    assert!(cache.is_empty());
}

#[tokio::test]
async fn the_cache_is_bounded() {
    let cache = ResponseCache::new().max_entries(2);
    let (app, _) = app(cache.clone());
    let s = spawn(app).await;
    for q in ["/lang?a=1", "/lang?a=2", "/lang?a=3", "/lang?a=4"] {
        get(&s, q).await;
    }
    assert!(cache.len() <= 2);
    // the newest survived
    assert_eq!(get(&s, "/lang?a=4").await.1, "HIT");
}

#[tokio::test]
async fn very_long_keys_bypass_the_cache() {
    let cache = ResponseCache::new();
    let (app, calls) = app(cache.clone());
    let s = spawn(app).await;
    let long = format!("/lang?q={}", "a".repeat(3000));
    for _ in 0..2 {
        let r = s.get(&long).send().await;
        assert_eq!(r.status, 200);
        assert!(!r.headers.contains_key("x-cache"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(cache.is_empty());
}
