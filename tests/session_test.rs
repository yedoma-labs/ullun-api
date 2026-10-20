//! Sessions and CSRF protection over real HTTP

mod common;

use common::{spawn, Reply, Server};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use ullun::csrf::Csrf;
use ullun::prelude::*;
use ullun::session::{MemoryStore, SessionLayer};

const COOKIE: &str = "__Host-sid";

struct Fixture {
    server: Server,
}

fn layer(clock: &Arc<AtomicU64>) -> SessionLayer {
    let c = clock.clone();
    SessionLayer::new().clock(move || c.load(Ordering::SeqCst))
}

fn routes(app: App) -> App {
    app.get("/whoami", |req: Request| async move {
        let s = req.session()?;
        let user: Option<String> = s.get("user");
        let hits: u32 = s.get("hits").unwrap_or(0);
        Ok(Response::json(
            serde_json::json!({ "user": user, "hits": hits, "established": s.is_established() }),
        ))
    })
    .post("/login/{name}", |req: Request| async move {
        let s = req.session()?;
        s.renew();
        s.set("user", req.params.get("name")?)?;
        Ok(Response::text("ok"))
    })
    .post("/hit", |req: Request| async move {
        let s = req.session()?;
        let n: u32 = s.get("hits").unwrap_or(0) + 1;
        s.set("hits", n)?;
        Ok(Response::text(n.to_string()))
    })
    .post("/logout", |req: Request| async move {
        req.session()?.destroy();
        Ok(Response::text("bye"))
    })
    .post("/flash", |req: Request| async move {
        req.session()?.flash("saved!")?;
        Ok(Response::redirect("/flashes"))
    })
    .get("/flashes", |req: Request| async move {
        Ok(Response::json(serde_json::json!(req
            .session()?
            .take_flash())))
    })
    .post("/fail", |req: Request| async move {
        req.session()?.set("touched", true)?;
        Err::<Response, _>(Error::bad_request("nope"))
    })
}

async fn fixture() -> Fixture {
    let clock = Arc::new(AtomicU64::new(100_000));
    let server = spawn(routes(App::new().middleware(layer(&clock)))).await;
    Fixture { server }
}

fn who(res: &Reply) -> serde_json::Value {
    res.json()
}

#[tokio::test]
async fn anonymous_visitors_get_no_cookie_and_writers_get_a_hardened_one() {
    let f = fixture().await;
    let anon = f.server.get("/whoami").send().await;
    assert!(
        anon.set_cookie(COOKIE).is_none(),
        "no cookie without session data"
    );
    assert_eq!(who(&anon)["established"], false);

    let login = f.server.post("/login/ann").send().await;
    let cookie = login.set_cookie(COOKIE).expect("cookie after writing");
    let id = login.cookie(COOKIE).unwrap();
    assert_eq!(id.len(), 64);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    for attr in [
        "Path=/",
        "Max-Age=1800",
        "Secure",
        "HttpOnly",
        "SameSite=Lax",
    ] {
        assert!(cookie.contains(attr), "{attr} missing in {cookie}");
    }
    assert!(!cookie.contains("Domain"));
}

#[tokio::test]
async fn data_persists_per_session_and_sessions_are_isolated() {
    let f = fixture().await;
    let a = f
        .server
        .post("/login/ann")
        .send()
        .await
        .cookie(COOKIE)
        .unwrap();
    let b = f
        .server
        .post("/login/bob")
        .send()
        .await
        .cookie(COOKIE)
        .unwrap();
    assert_ne!(a, b);

    for _ in 0..3 {
        f.server.post("/hit").cookie(COOKIE, &a).send().await;
    }
    f.server.post("/hit").cookie(COOKIE, &b).send().await;

    let ann = who(&f.server.get("/whoami").cookie(COOKIE, &a).send().await);
    let bob = who(&f.server.get("/whoami").cookie(COOKIE, &b).send().await);
    assert_eq!(
        (ann["user"].as_str(), ann["hits"].as_u64()),
        (Some("ann"), Some(3))
    );
    assert_eq!(
        (bob["user"].as_str(), bob["hits"].as_u64()),
        (Some("bob"), Some(1))
    );
}

#[tokio::test]
async fn forged_unknown_and_malformed_cookies_are_ignored() {
    let f = fixture().await;
    f.server.post("/login/ann").send().await;

    for bad in [
        "",
        "garbage",
        &"0".repeat(64),
        &"A".repeat(64),
        &"a".repeat(63),
        "../../etc/passwd",
        &"a".repeat(65),
    ] {
        let res = f.server.get("/whoami").cookie(COOKIE, bad).send().await;
        assert_eq!(who(&res)["user"], serde_json::Value::Null, "{bad:?}");
        assert_eq!(who(&res)["established"], false);
    }
    // a well-formed but unknown id gets its cookie cleared
    let unknown = f
        .server
        .get("/whoami")
        .cookie(COOKIE, &"0".repeat(64))
        .send()
        .await;
    assert_eq!(unknown.cookie(COOKIE).as_deref(), Some(""));
    assert!(unknown.set_cookie(COOKIE).unwrap().contains("Max-Age=0"));
}

#[tokio::test]
async fn renewing_the_id_on_login_defeats_session_fixation() {
    let f = fixture().await;
    // the attacker's session, planted in the victim's browser
    let planted = f.server.post("/hit").send().await.cookie(COOKIE).unwrap();
    assert_eq!(
        who(&f
            .server
            .get("/whoami")
            .cookie(COOKIE, &planted)
            .send()
            .await)["hits"],
        1
    );

    // the victim logs in using the planted cookie
    let login = f
        .server
        .post("/login/victim")
        .cookie(COOKIE, &planted)
        .send()
        .await;
    let fresh = login.cookie(COOKIE).unwrap();
    assert_ne!(fresh, planted);

    // data came along, the planted id is dead
    let victim = who(&f.server.get("/whoami").cookie(COOKIE, &fresh).send().await);
    assert_eq!(
        (victim["user"].as_str(), victim["hits"].as_u64()),
        (Some("victim"), Some(1))
    );
    let attacker = who(&f
        .server
        .get("/whoami")
        .cookie(COOKIE, &planted)
        .send()
        .await);
    assert_eq!(attacker["user"], serde_json::Value::Null);
}

#[tokio::test]
async fn logout_destroys_the_session() {
    let f = fixture().await;
    let id = f
        .server
        .post("/login/ann")
        .send()
        .await
        .cookie(COOKIE)
        .unwrap();
    let out = f.server.post("/logout").cookie(COOKIE, &id).send().await;
    assert_eq!(out.cookie(COOKIE).as_deref(), Some(""));
    assert_eq!(
        who(&f.server.get("/whoami").cookie(COOKIE, &id).send().await)["user"],
        serde_json::Value::Null
    );
    // logging out without a session does not invent one
    assert!(f
        .server
        .post("/logout")
        .send()
        .await
        .set_cookie(COOKIE)
        .is_none());
}

#[tokio::test]
async fn idle_timeout_slides_with_activity_and_absolute_timeout_does_not() {
    let clock = Arc::new(AtomicU64::new(1_000));
    let c = clock.clone();
    let server = spawn(routes(
        App::new().middleware(
            SessionLayer::new()
                .idle_timeout(1_800)
                .absolute_timeout(5_000)
                .clock(move || c.load(Ordering::SeqCst)),
        ),
    ))
    .await;
    let user = |id: String| {
        let call = server.get("/whoami").cookie(COOKIE, &id);
        async move { who(&call.send().await)["user"].as_str().map(str::to_owned) }
    };

    let id = server
        .post("/login/ann")
        .send()
        .await
        .cookie(COOKIE)
        .unwrap();
    // quiet for less than the idle timeout: alive, and the cookie is refreshed
    clock.store(1_000 + 1_000, Ordering::SeqCst);
    let refreshed = server.get("/whoami").cookie(COOKIE, &id).send().await;
    assert!(
        refreshed.set_cookie(COOKIE).is_some(),
        "idle timer should be extended"
    );
    // ... which keeps it alive well past one idle period after creation
    clock.store(1_000 + 2_500, Ordering::SeqCst);
    assert_eq!(user(id.clone()).await.as_deref(), Some("ann"));

    // keep it busy until the absolute limit (created at 1000, limit 5000 -> 6000)
    for t in [4_500u64, 5_500, 5_900] {
        clock.store(t, Ordering::SeqCst);
        assert_eq!(user(id.clone()).await.as_deref(), Some("ann"), "t+{t}");
    }
    clock.store(6_000, Ordering::SeqCst);
    assert_eq!(
        user(id.clone()).await,
        None,
        "absolute timeout ends even active sessions"
    );

    // idle expiry
    let id2 = server
        .post("/login/bob")
        .send()
        .await
        .cookie(COOKIE)
        .unwrap();
    clock.store(6_000 + 1_799, Ordering::SeqCst);
    assert_eq!(user(id2.clone()).await.as_deref(), Some("bob"));
    clock.store(6_000 + 1_799 + 1_800, Ordering::SeqCst);
    assert_eq!(user(id2).await, None);
}

#[tokio::test]
async fn flash_messages_are_shown_exactly_once() {
    let f = fixture().await;
    let id = f.server.post("/flash").send().await.cookie(COOKIE).unwrap();
    let first = f.server.get("/flashes").cookie(COOKIE, &id).send().await;
    assert_eq!(first.json(), serde_json::json!(["saved!"]));
    let second = f.server.get("/flashes").cookie(COOKIE, &id).send().await;
    assert_eq!(second.json(), serde_json::json!([]));
}

#[tokio::test]
async fn changes_are_kept_even_when_the_handler_fails() {
    let f = fixture().await;
    let res = f.server.post("/fail").send().await;
    assert_eq!(res.status, 400);
    assert!(res.cookie(COOKIE).is_some());
}

#[tokio::test]
async fn insecure_cookie_mode_for_plain_http_development() {
    let clock = Arc::new(AtomicU64::new(1));
    let server = spawn(routes(
        App::new().middleware(layer(&clock).insecure_cookies()),
    ))
    .await;
    let res = server.post("/login/ann").send().await;
    assert!(res.cookie(COOKIE).is_none());
    let cookie = res.set_cookie("sid").expect("plain cookie name");
    assert!(!cookie.contains("Secure") && cookie.contains("HttpOnly"));
    let id = res.cookie("sid").unwrap();
    assert_eq!(
        who(&server.get("/whoami").cookie("sid", &id).send().await)["user"],
        "ann"
    );
}

#[tokio::test]
async fn a_missing_layer_is_a_500_not_silently_unprotected() {
    let server = spawn(App::new().get("/x", |req: Request| async move {
        req.session()?;
        Ok(Response::text("unreachable"))
    }))
    .await;
    let res = server.get("/x").send().await;
    assert_eq!(res.status, 500);
    assert!(res.text().contains("SessionLayer"));
}

#[tokio::test]
async fn a_small_store_evicts_old_sessions_instead_of_growing() {
    let clock = Arc::new(AtomicU64::new(10));
    let c = clock.clone();
    let server = spawn(routes(
        App::new().middleware(
            SessionLayer::with_store(MemoryStore::with_capacity(2))
                .clock(move || c.load(Ordering::SeqCst)),
        ),
    ))
    .await;
    let mut ids = Vec::new();
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        clock.store(10 + i as u64, Ordering::SeqCst);
        ids.push(
            server
                .post(&format!("/login/{name}"))
                .send()
                .await
                .cookie(COOKIE)
                .unwrap(),
        );
    }
    assert_eq!(
        who(&server.get("/whoami").cookie(COOKIE, &ids[0]).send().await)["user"],
        serde_json::Value::Null
    );
    assert_eq!(
        who(&server.get("/whoami").cookie(COOKIE, &ids[2]).send().await)["user"],
        "c"
    );
}

// ---- CSRF ----

fn csrf_app(csrf: Csrf) -> (App, Arc<AtomicU64>) {
    let clock = Arc::new(AtomicU64::new(1));
    let app = App::new()
        .middleware(layer(&clock))
        .middleware(csrf)
        .get("/form", |req: Request| async move {
            Ok(Response::text(req.session()?.csrf_token()?))
        })
        .post("/save", |_req: Request| async {
            Ok(Response::text("saved"))
        })
        .delete("/item", |_req: Request| async {
            Ok(Response::text("deleted"))
        });
    (app, clock)
}

/// A session with a CSRF token: (session cookie, token)
async fn session_with_token(s: &Server) -> (String, String) {
    let res = s.get("/form").send().await;
    (res.cookie(COOKIE).expect("session cookie"), res.text())
}

#[tokio::test]
async fn unsafe_methods_need_the_session_token() {
    let (app, _) = csrf_app(Csrf::new());
    let s = spawn(app).await;
    let (id, token) = session_with_token(&s).await;
    assert_eq!(token.len(), 64);

    // safe methods never need one
    assert_eq!(s.get("/form").send().await.status, 200);
    assert_eq!(s.request("OPTIONS", "/save").send().await.status, 204);

    // without a token, with a wrong one, and without any session
    assert_eq!(s.post("/save").cookie(COOKIE, &id).send().await.status, 403);
    assert_eq!(
        s.post("/save")
            .cookie(COOKIE, &id)
            .header("x-csrf-token", "nope")
            .send()
            .await
            .status,
        403
    );
    assert_eq!(
        s.post("/save")
            .cookie(COOKIE, &id)
            .header("x-csrf-token", &token[..63])
            .send()
            .await
            .status,
        403
    );
    assert_eq!(
        s.post("/save")
            .header("x-csrf-token", &token)
            .send()
            .await
            .status,
        403
    );
    let denied = s.delete("/item").cookie(COOKIE, &id).send().await;
    assert_eq!(denied.status, 403);
    assert!(denied.text().contains("CSRF"));

    // a token from another session does not work
    let (_, other_token) = session_with_token(&s).await;
    assert_ne!(token, other_token);
    assert_eq!(
        s.post("/save")
            .cookie(COOKIE, &id)
            .header("x-csrf-token", &other_token)
            .send()
            .await
            .status,
        403
    );

    // header, urlencoded field and multipart field all work
    assert_eq!(
        s.post("/save")
            .cookie(COOKIE, &id)
            .header("x-csrf-token", &token)
            .send()
            .await
            .status,
        200
    );
    assert_eq!(
        s.delete("/item")
            .cookie(COOKIE, &id)
            .header("x-csrf-token", &token)
            .send()
            .await
            .status,
        200
    );
    let form = format!("title=x&csrf_token={token}");
    assert_eq!(
        s.post("/save")
            .cookie(COOKIE, &id)
            .form(&form)
            .send()
            .await
            .status,
        200
    );
    assert_eq!(
        s.post("/save")
            .cookie(COOKIE, &id)
            .form("title=x&csrf_token=wrong")
            .send()
            .await
            .status,
        403
    );
    let multipart = s.post("/save").cookie(COOKIE, &id).multipart(&[
        common::Field::text("title", "x"),
        common::Field::text("csrf_token", &token),
    ]);
    assert_eq!(multipart.send().await.status, 200);
    // JSON bodies cannot carry it in the body: header only
    let json = s
        .post("/save")
        .cookie(COOKIE, &id)
        .json(&serde_json::json!({ "csrf_token": token }));
    assert_eq!(json.send().await.status, 403);
}

#[tokio::test]
async fn cross_site_origins_are_blocked_even_with_a_valid_token() {
    let (app, _) = csrf_app(Csrf::new().allow_origin("https://admin.example.com"));
    let s = spawn(app).await;
    let (id, token) = session_with_token(&s).await;
    let post = |origin: Option<&str>, fetch_site: Option<&str>| {
        let mut call = s
            .post("/save")
            .cookie(COOKIE, &id)
            .header("x-csrf-token", &token);
        if let Some(o) = origin {
            call = call.header("origin", o);
        }
        if let Some(f) = fetch_site {
            call = call.header("sec-fetch-site", f);
        }
        async move { call.send().await.status }
    };
    // the test client sends `Host: localhost`, so that is "our" origin
    let own = "http://localhost".to_string();

    assert_eq!(post(Some(&own), None).await, 200);
    assert_eq!(post(Some("https://admin.example.com"), None).await, 200);
    assert_eq!(post(Some("https://evil.example"), None).await, 403);
    assert_eq!(post(Some("null"), None).await, 403);
    assert_eq!(post(Some("http://localhost.evil.example"), None).await, 403);
    // Fetch Metadata when there is no Origin
    assert_eq!(post(None, Some("cross-site")).await, 403);
    assert_eq!(post(None, Some("same-site")).await, 403);
    assert_eq!(post(None, Some("same-origin")).await, 200);
    assert_eq!(post(None, Some("none")).await, 200);
    // non-browser clients send neither
    assert_eq!(post(None, None).await, 200);
}

#[tokio::test]
async fn bearer_authenticated_requests_can_be_exempted() {
    let (app, _) = csrf_app(Csrf::new().exempt(|req| req.headers.contains_key("authorization")));
    let s = spawn(app).await;
    assert_eq!(s.post("/save").bearer("token").send().await.status, 200);
    assert_eq!(s.post("/save").send().await.status, 403);
}

#[tokio::test]
async fn csrf_without_sessions_is_a_500() {
    let s = spawn(
        App::new()
            .middleware(Csrf::new())
            .post("/save", |_req: Request| async { Ok(Response::text("x")) }),
    )
    .await;
    assert_eq!(s.post("/save").send().await.status, 500);
}
