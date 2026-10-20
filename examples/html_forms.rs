//! Scenario: server-rendered HTML with forms, sessions and flash messages
//!
//! - HTML pages from plain Rust strings (all user input escaped)
//! - `application/x-www-form-urlencoded` parsing with `Request::form`
//! - Post/Redirect/Get with a one-shot flash cookie
//! - cookie session guarding `/dashboard`
//!
//! The session store is in memory and the "password" is a demo; use a real
//! hasher (argon2) and a persistent store in production.
//!
//! Port: set PORT (default 3000)
//! Run:  cargo run --example html_forms   then open http://localhost:3000

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex};
use ullun::cookies::SameSite;
use ullun::prelude::*;

type Sessions = Arc<Mutex<HashMap<String, String>>>;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn page(title: &str, flash: Option<&str>, body: &str) -> Response {
    let flash = flash
        .map(|m| format!("<p style=\"color:#b00\">{}</p>", escape(m)))
        .unwrap_or_default();
    Response::ok()
        .with_header("content-type", "text/html; charset=utf-8")
        .with_body(format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{t}</title>\
             <body style=\"font-family:system-ui;margin:3rem\"><h1>{t}</h1>{flash}{body}</body>",
            t = escape(title)
        ))
}

fn redirect(to: &str) -> Response {
    Response::new(http::StatusCode::SEE_OTHER).with_header("location", to)
}

#[derive(Deserialize)]
struct LoginForm {
    user: String,
    password: String,
}

/// Unguessable session id from std's randomly seeded SipHash. Good enough for
/// a demo; use a CSPRNG (`getrandom`, `rand`) for real sessions.
fn new_session_id() -> String {
    let rnd = |_| {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    };
    format!("{:016x}{:016x}", rnd(0), rnd(1))
}

fn flash_cookie(msg: &str) -> Cookie {
    // cookie values may not contain ';' or control characters
    let safe: String = msg
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .collect();
    Cookie::new("flash", safe.replace(' ', "_"))
        .max_age(10)
        .http_only()
}

fn take_flash(req: &Request) -> Option<String> {
    req.cookies().get("flash").map(|v| v.replace('_', " "))
}

fn clear_flash() -> Cookie {
    Cookie::new("flash", "").max_age(0)
}

#[tokio::main]
async fn main() {
    let sessions: Sessions = Arc::default();

    let s = sessions.clone();
    let login_page = |req: Request| async move {
        let flash = take_flash(&req);
        Ok(page(
            "Sign in",
            flash.as_deref(),
            "<form method=\"post\" action=\"/login\">\
             <input name=\"user\" placeholder=\"name\" required> \
             <input name=\"password\" type=\"password\" placeholder=\"password (demo)\" required> \
             <button>Sign in</button></form>",
        )
        .cookie(clear_flash()))
    };

    let login = move |req: Request| {
        let s = s.clone();
        async move {
            let f: LoginForm = req.form()?;
            let user = f.user.trim().to_string();
            if user.is_empty() || f.password != "demo" {
                return Ok(redirect("/").cookie(flash_cookie("Wrong name or password")));
            }
            let sid = new_session_id();
            s.lock()
                .expect("session lock poisoned")
                .insert(sid.clone(), user);
            Ok(redirect("/dashboard").cookie(
                Cookie::new("sid", sid)
                    .http_only()
                    .same_site(SameSite::Lax)
                    .max_age(3600),
            ))
        }
    };

    let s = sessions.clone();
    let dashboard = move |req: Request| {
        let s = s.clone();
        async move {
            let user = req
                .cookies()
                .get("sid")
                .and_then(|sid| s.lock().expect("session lock poisoned").get(sid).cloned());
            match user {
                Some(user) => Ok(page(
                    &format!("Hello, {user}"),
                    None,
                    "<form method=\"post\" action=\"/logout\"><button>Sign out</button></form>",
                )),
                None => Ok(redirect("/").cookie(flash_cookie("Please sign in first"))),
            }
        }
    };

    let s = sessions;
    let logout = move |req: Request| {
        let s = s.clone();
        async move {
            if let Some(sid) = req.cookies().get("sid") {
                s.lock().expect("session lock poisoned").remove(sid);
            }
            Ok(redirect("/").cookie(Cookie::new("sid", "").max_age(0)))
        }
    };

    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );

    App::new()
        .max_body_size(Some(16 * 1024))
        .get("/", login_page)
        .post("/login", login)
        .get("/dashboard", dashboard)
        .post("/logout", logout)
        .run(&addr)
        .await
        .unwrap();
}
