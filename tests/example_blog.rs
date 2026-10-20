//! The blog example, tested like a browser would use it: cookies, forms, CSRF tokens

mod common;

#[allow(dead_code)]
#[path = "../examples/blog/app.rs"]
mod blog;

use argon2::password_hash::PasswordHasher;
use argon2::{Algorithm, Argon2, Params, Version};
use common::{spawn, Reply, Server};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PASSWORD: &str = "correct horse battery staple";

fn fast_hash(password: &str) -> String {
    let hasher = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(8, 1, 1, None).unwrap(),
    );
    hasher
        .hash_password(password.as_bytes())
        .unwrap()
        .to_string()
}

fn config(clock: Arc<AtomicU64>) -> blog::Config {
    blog::Config {
        site_title: "Test <Blog>".into(),
        base_url: "https://blog.example/".into(),
        admin_user: "ann".into(),
        admin_password_hash: fast_hash(PASSWORD),
        data_file: None,
        secure_cookies: false, // plain http in tests: cookie name "sid"
        dev: false,
        login_attempts: (1000, Duration::from_secs(60)),
        fast_hashing: true,
        clock: Arc::new(move || clock.load(Ordering::SeqCst)),
    }
}

/// A browser: keeps the session cookie between requests
struct Browser {
    server: Server,
    cookie: Mutex<Option<String>>,
}

impl Browser {
    fn new(server: Server) -> Self {
        Self {
            server,
            cookie: Mutex::new(None),
        }
    }

    fn session_id(&self) -> Option<String> {
        self.cookie.lock().unwrap().clone()
    }

    fn remember(&self, res: &Reply) {
        if let Some(c) = res.cookie("sid") {
            let mut jar = self.cookie.lock().unwrap();
            *jar = if c.is_empty() { None } else { Some(c) };
        }
    }

    async fn get(&self, path: &str) -> Reply {
        let mut call = self.server.get(path).header("accept", "text/html");
        if let Some(id) = self.session_id() {
            call = call.cookie("sid", &id);
        }
        let res = call.send().await;
        self.remember(&res);
        res
    }

    async fn post(&self, path: &str, form: &str) -> Reply {
        let mut call = self
            .server
            .post(path)
            .header("accept", "text/html")
            .form(form);
        if let Some(id) = self.session_id() {
            call = call.cookie("sid", &id);
        }
        let res = call.send().await;
        self.remember(&res);
        res
    }

    /// The CSRF token of the page at `path`
    async fn token(&self, path: &str) -> String {
        let page = self.get(path).await.text();
        token_in(&page)
    }

    /// POST a form with the token of the page at `token_page`
    async fn post_form(&self, path: &str, token_page: &str, fields: &str) -> Reply {
        let token = self.token(token_page).await;
        self.post(path, &format!("csrf_token={token}&{fields}"))
            .await
    }

    async fn login(&self) {
        let res = self
            .post_form(
                "/admin/login",
                "/admin/login",
                &format!("username=ann&password={}", urlencoding::encode(PASSWORD)),
            )
            .await;
        assert_eq!(res.status, 303, "{}", res.text());
        assert_eq!(res.header("location"), "/admin");
    }
}

/// Id of the first post row of the dashboard (the `/edit` links carry the ids)
fn first_post_id(dashboard: &str) -> String {
    let marker = "/admin/posts/";
    let mut rest = dashboard;
    while let Some(i) = rest.find(marker) {
        rest = &rest[i + marker.len()..];
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !id.is_empty() && rest[id.len()..].starts_with("/edit") {
            return id;
        }
    }
    panic!("no post row in: {dashboard}");
}

fn token_in(html: &str) -> String {
    let marker = "name=\"csrf_token\" value=\"";
    let start = html
        .find(marker)
        .unwrap_or_else(|| panic!("no csrf token in: {html}"))
        + marker.len();
    html[start..start + 64].to_string()
}

async fn fixture() -> (Browser, Server) {
    let clock = Arc::new(AtomicU64::new(1_700_000_000));
    let server = spawn(blog::build(config(clock))).await;
    (Browser::new(server), server)
}

async fn publish(b: &Browser, title: &str, body: &str, tags: &str) -> Reply {
    let fields = format!(
        "title={}&body={}&tags={}&publish=1",
        urlencoding::encode(title),
        urlencoding::encode(body),
        urlencoding::encode(tags)
    );
    b.post_form("/admin/posts", "/admin/posts/new", &fields)
        .await
}

fn slug_from(res: &Reply) -> String {
    assert_eq!(res.status, 303, "{}", res.text());
    res.header("location")
        .strip_prefix("/posts/")
        .expect("redirect to the post")
        .to_string()
}

#[tokio::test]
async fn readers_get_pages_without_cookies_and_with_strict_headers() {
    let (b, server) = fixture().await;
    let home = b.get("/").await;
    assert_eq!(home.status, 200);
    assert!(
        home.set_cookie("sid").is_none(),
        "anonymous readers must not get a session"
    );
    assert!(home.text().contains("No posts."));
    assert!(
        home.text().contains("Test &lt;Blog&gt;"),
        "site title is escaped"
    );
    let csp = home.header("content-security-policy");
    assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
    assert!(!csp.contains("unsafe-inline") && !csp.contains("script-src"));
    assert_eq!(home.header("x-content-type-options"), "nosniff");
    assert_eq!(home.header("x-frame-options"), "DENY");
    assert_eq!(home.header("strict-transport-security"), ""); // only with secure cookies

    assert!(server
        .get("/style.css")
        .send()
        .await
        .header("content-type")
        .starts_with("text/css"));
    assert!(server
        .get("/robots.txt")
        .send()
        .await
        .text()
        .contains("Disallow: /admin"));
    let feed = server.get("/feed.xml").send().await;
    assert_eq!(
        feed.header("content-type"),
        "application/rss+xml; charset=utf-8"
    );
    assert!(
        feed.text().starts_with("<?xml")
            && feed.text().contains("<title>Test &lt;Blog&gt;</title>")
    );
    assert!(server
        .get("/sitemap.xml")
        .send()
        .await
        .text()
        .contains("<loc>https://blog.example/</loc>"));
}

#[tokio::test]
async fn the_admin_area_is_closed_to_visitors() {
    let (b, _) = fixture().await;
    for path in ["/admin", "/admin/posts/new", "/admin/posts/1/edit"] {
        let res = b.get(path).await;
        assert_eq!(
            (res.status, res.header("location")),
            (303, "/admin/login"),
            "{path}"
        );
    }
    // the redirect leaves a flash for the login page
    assert!(b
        .get("/admin/login")
        .await
        .text()
        .contains("Please sign in."));
    // writes without a token never get that far
    assert_eq!(b.post("/admin/posts", "title=x&body=y").await.status, 403);
    // with a token (the login page has one) but without being logged in: redirect to login
    let fields = "title=x&body=y";
    let res = b.post_form("/admin/posts", "/admin/login", fields).await;
    assert_eq!((res.status, res.header("location")), (303, "/admin/login"));
}

#[tokio::test]
async fn login_checks_the_password_renews_the_session_and_flashes() {
    let (b, _) = fixture().await;

    let before = {
        b.get("/admin/login").await;
        b.session_id()
            .expect("login page starts a session for its token")
    };

    let wrong = b
        .post_form("/admin/login", "/admin/login", "username=ann&password=nope")
        .await;
    assert_eq!(
        (wrong.status, wrong.header("location")),
        (303, "/admin/login")
    );
    assert!(b
        .get("/admin/login")
        .await
        .text()
        .contains("Wrong user name or password."));
    let wrong_user = b
        .post_form(
            "/admin/login",
            "/admin/login",
            &format!(
                "username=mallory&password={}",
                urlencoding::encode(PASSWORD)
            ),
        )
        .await;
    assert_eq!(wrong_user.header("location"), "/admin/login");

    b.login().await;
    let after = b.session_id().unwrap();
    assert_ne!(after, before, "the session id must change on login");

    let dash = b.get("/admin").await;
    assert_eq!(dash.status, 200);
    assert!(dash.text().contains("Welcome back.") && dash.text().contains("Dashboard"));
    assert_eq!(dash.header("cache-control"), "no-store");
    // already logged in: the login page forwards
    assert_eq!(b.get("/admin/login").await.header("location"), "/admin");
}

#[tokio::test]
async fn forms_are_csrf_protected_and_foreign_origins_are_refused() {
    let (b, server) = fixture().await;
    let creds = format!("username=ann&password={}", urlencoding::encode(PASSWORD));

    assert_eq!(b.post("/admin/login", &creds).await.status, 403);
    assert_eq!(
        b.post(
            "/admin/login",
            &format!("csrf_token={}&{creds}", "0".repeat(64))
        )
        .await
        .status,
        403
    );

    // a valid token from the page, but the request claims another origin
    let token = b.token("/admin/login").await;
    let id = b.session_id().unwrap();
    let evil = server
        .post("/admin/login")
        .cookie("sid", &id)
        .header("origin", "https://evil.example")
        .form(&format!("csrf_token={token}&{creds}"))
        .send()
        .await;
    assert_eq!(evil.status, 403);
    assert!(evil.text().contains("Cross-site"));

    // the same request from our own origin works
    let ok = server
        .post("/admin/login")
        .cookie("sid", &id)
        .header("origin", "http://localhost")
        .form(&format!("csrf_token={token}&{creds}"))
        .send()
        .await;
    assert_eq!(ok.status, 303);

    // a logged-in admin is still protected: no token, no delete
    let b = Browser::new(server);
    b.login().await;
    let slug = slug_from(&publish(&b, "Victim", "text", "").await);
    let id_of_post = first_post_id(&b.get("/admin").await.text());
    assert_eq!(
        b.post(&format!("/admin/posts/{id_of_post}/delete"), "")
            .await
            .status,
        403
    );
    assert_eq!(b.get(&format!("/posts/{slug}")).await.status, 200);
}

#[tokio::test]
async fn login_attempts_are_throttled() {
    let clock = Arc::new(AtomicU64::new(1));
    let mut cfg = config(clock);
    cfg.login_attempts = (3, Duration::from_secs(60));
    let server = spawn(blog::build(cfg)).await;
    let b = Browser::new(server);
    // loading the form any number of times costs nothing
    for _ in 0..10 {
        assert_eq!(b.get("/admin/login").await.status, 200);
    }
    for i in 0..3 {
        let res = b
            .post_form(
                "/admin/login",
                "/admin/login",
                "username=ann&password=guess",
            )
            .await;
        assert_eq!(res.status, 303, "attempt {i}");
    }
    let blocked = b
        .post_form(
            "/admin/login",
            "/admin/login",
            "username=ann&password=guess",
        )
        .await;
    assert_eq!(blocked.status, 429);
    assert!(
        blocked.text().contains("<h1>429</h1>"),
        "browsers get an HTML page: {}",
        blocked.text()
    );
    assert!(blocked.header("retry-after").parse::<u64>().unwrap() >= 1);
    // the public blog is unaffected
    assert_eq!(b.get("/").await.status, 200);
}

#[tokio::test]
async fn drafts_are_private_until_published() {
    let (b, server) = fixture().await;
    b.login().await;

    let fields = "title=Hello+world&body=First+post&tags=rust%2C+intro";
    let saved = b
        .post_form("/admin/posts", "/admin/posts/new", fields)
        .await;
    let slug = slug_from(&saved);
    assert_eq!(slug, "hello-world");

    // visible to the admin (with a draft marker), invisible to everyone else
    let preview = b.get(&format!("/posts/{slug}")).await;
    assert_eq!(preview.status, 200);
    assert!(
        preview.text().contains("draft (only you can see this)")
            && preview.text().contains("Saved as draft.")
    );
    assert_eq!(preview.header("cache-control"), "no-store");

    let anon = Browser::new(server);
    let hidden = anon.get(&format!("/posts/{slug}")).await;
    assert_eq!(hidden.status, 404);
    assert!(
        hidden.text().contains("<h1>404</h1>"),
        "browsers get an HTML error page"
    );
    assert!(hidden.header("content-type").starts_with("text/html"));
    for path in ["/", "/tags/rust", "/feed.xml", "/sitemap.xml"] {
        let text = anon.get(path).await.text();
        assert!(
            !text.contains("Hello world") && !text.contains("hello-world"),
            "{path} leaks a draft"
        );
    }
    // API clients (no text/html in Accept) keep JSON errors
    assert_eq!(
        server.get(&format!("/posts/{slug}")).send().await.json()["status"],
        404
    );

    // publish from the dashboard
    let dash = b.get("/admin").await.text();
    assert!(dash.contains("draft") && dash.contains("publish"));
    let id = first_post_id(&dash);
    let res = b
        .post_form(&format!("/admin/posts/{id}/publish"), "/admin", "")
        .await;
    assert_eq!((res.status, res.header("location")), (303, "/admin"));

    assert!(anon.get("/").await.text().contains("Hello world"));
    assert_eq!(anon.get(&format!("/posts/{slug}")).await.status, 200);
    assert!(anon.get("/tags/rust").await.text().contains("Hello world"));
    assert!(server
        .get("/feed.xml")
        .send()
        .await
        .text()
        .contains("<link>https://blog.example/posts/hello-world</link>"));
    assert!(server
        .get("/sitemap.xml")
        .send()
        .await
        .text()
        .contains("https://blog.example/posts/hello-world"));

    // and back to a draft
    b.post_form(&format!("/admin/posts/{id}/publish"), "/admin", "")
        .await;
    assert_eq!(anon.get(&format!("/posts/{slug}")).await.status, 404);
}

#[tokio::test]
async fn posts_cannot_inject_script_or_markup() {
    let (b, server) = fixture().await;
    b.login().await;
    let body = "Intro\n\n<script>alert('body')</script>\n\n<img src=x onerror=alert(1)>\n\n[click](javascript:alert(1)) and ![pic](javascript:alert(2))\n\n[fine](https://example.com/ok)";
    let slug = slug_from(&publish(&b, "<script>alert('title')</script> & more", body, "xss").await);

    let anon = Browser::new(server);
    for path in [
        format!("/posts/{slug}"),
        "/".to_string(),
        "/tags/xss".to_string(),
        "/feed.xml".to_string(),
    ] {
        let res = anon.get(&path).await;
        let text = res.text();
        assert_eq!(res.status, 200, "{path}");
        assert!(!text.contains("<script>alert"), "{path}: {text}");
        assert!(!text.contains("<img src=x"), "{path}");
        assert!(!text.contains("javascript:"), "{path}");
    }
    let page = anon.get(&format!("/posts/{slug}")).await.text();
    assert!(
        page.contains("&lt;script&gt;alert(&#39;title&#39;)&lt;/script&gt; &amp; more")
            || page.contains("&lt;script&gt;alert(")
    );
    assert!(page.contains("href=\"https://example.com/ok\"") && page.contains("href=\"#\""));
    // the feed is well-formed XML text
    let feed = anon.get("/feed.xml").await.text();
    assert!(feed.contains("&lt;script&gt;") && !feed.contains("<script"));
}

#[tokio::test]
async fn the_editor_reports_all_problems_and_keeps_the_input() {
    let (b, _) = fixture().await;
    b.login().await;

    let bad = format!(
        "title={}&body=%20%20&tags={}",
        "t".repeat(121),
        urlencoding::encode("ok, Bad Tag!, a,b,c,d,e,f,g,h")
    );
    let res = b.post_form("/admin/posts", "/admin/posts/new", &bad).await;
    assert_eq!(res.status, 422);
    let html = res.text();
    assert!(
        html.contains("title: must be 1 to 120 characters long"),
        "{html}"
    );
    assert!(html.contains("body: must be 1 to 50000 characters long"));
    assert!(html.contains("tags: at most 8 tags") && html.contains("tags may use a-z"));
    assert!(html.contains(&"t".repeat(121)), "the input is kept");
    // nothing was saved
    assert!(!b.get("/admin").await.text().contains(&"t".repeat(20)));

    // missing form fields are a plain 400
    assert_eq!(
        b.post_form("/admin/posts", "/admin/posts/new", "title=only")
            .await
            .status,
        400
    );
}

#[tokio::test]
async fn slugs_are_unique_and_edits_keep_them() {
    let (b, server) = fixture().await;
    b.login().await;

    let first = slug_from(&publish(&b, "Same Title!", "one", "").await);
    let second = slug_from(&publish(&b, "Same Title!", "two", "").await);
    let third = slug_from(&publish(&b, "日本語", "three", "").await);
    let fourth = slug_from(&publish(&b, "  --Clean__Me--  ", "four", "").await);
    assert_eq!(
        (
            first.as_str(),
            second.as_str(),
            third.as_str(),
            fourth.as_str()
        ),
        ("same-title", "same-title-2", "post", "clean-me")
    );

    // edit: new title and text, same URL, publication date kept
    let dash = b.get("/admin").await.text();
    let edit_link = first_post_id(&dash);
    let edit_page = b.get(&format!("/admin/posts/{edit_link}/edit")).await;
    assert_eq!(edit_page.status, 200);
    let res = b
        .post_form(
            &format!("/admin/posts/{edit_link}"),
            &format!("/admin/posts/{edit_link}/edit"),
            "title=Renamed&body=changed&tags=a&publish=1",
        )
        .await;
    let kept_slug = slug_from(&res);
    assert!(["same-title", "same-title-2", "post", "clean-me"].contains(&kept_slug.as_str()));
    let page = Browser::new(server)
        .get(&format!("/posts/{kept_slug}"))
        .await
        .text();
    assert!(page.contains("Renamed") && page.contains("changed"));

    assert_eq!(b.get("/admin/posts/999/edit").await.status, 404);
    assert_eq!(b.get("/admin/posts/abc/edit").await.status, 400);
}

#[tokio::test]
async fn listing_paginates_searches_and_filters_by_tag() {
    let (b, server) = fixture().await;
    b.login().await;
    for i in 1..=7 {
        let tags = if i % 2 == 0 { "even" } else { "odd" };
        publish(
            &b,
            &format!("Post number {i}"),
            &format!("Body about topic{}", i % 3),
            tags,
        )
        .await;
    }
    let reader = Browser::new(server);

    let page1 = reader.get("/").await.text();
    assert_eq!(page1.matches("<article>").count(), 5);
    assert!(page1.contains("older →") && !page1.contains("← newer"));
    // newest first
    assert!(page1.find("Post number 7").unwrap() < page1.find("Post number 6").unwrap());
    let page2 = reader.get("/?page=2").await.text();
    assert_eq!(page2.matches("<article>").count(), 2);
    assert!(page2.contains("← newer") && !page2.contains("older →"));
    assert_eq!(
        reader
            .get("/?page=99")
            .await
            .text()
            .matches("<article>")
            .count(),
        0
    );
    assert_eq!(reader.get("/?page=abc").await.status, 400);

    let search = reader.get("/?q=TOPIC1").await.text();
    assert_eq!(search.matches("<article>").count(), 3); // posts 1, 4, 7
    assert!(search.contains("value=\"topic1\""));
    let tagged = reader.get("/tags/even").await.text();
    assert_eq!(tagged.matches("<article>").count(), 3);
    assert!(tagged.contains("Posts tagged “even”"));
    // reflected input is escaped
    let xss = reader
        .get("/?q=%22%3E%3Cscript%3Ealert(1)%3C/script%3E")
        .await
        .text();
    assert!(!xss.contains("<script>alert") && xss.contains("&lt;script&gt;"));
    let tag_xss = reader.get("/tags/%3Cscript%3E").await.text();
    assert!(!tag_xss.contains("<script>"));
}

#[tokio::test]
async fn the_index_supports_conditional_requests() {
    let (b, server) = fixture().await;
    let reader = Browser::new(server);
    let first = reader.get("/").await;
    let etag = first.header("etag").to_string();
    assert!(!etag.is_empty());
    let cached = server.get("/").header("if-none-match", &etag).send().await;
    assert_eq!((cached.status, cached.body.len()), (304, 0));

    b.login().await;
    publish(&b, "New", "text", "").await;
    let changed = server.get("/").header("if-none-match", &etag).send().await;
    assert_eq!(changed.status, 200);
}

#[tokio::test]
async fn deleting_and_logging_out() {
    let (b, server) = fixture().await;
    b.login().await;
    let slug = slug_from(&publish(&b, "Temporary", "text", "").await);
    let dash = b.get("/admin").await.text();
    let id = first_post_id(&dash);

    let res = b
        .post_form(&format!("/admin/posts/{id}/delete"), "/admin", "")
        .await;
    assert_eq!(res.status, 303);
    assert!(b.get("/admin").await.text().contains("Post deleted."));
    assert_eq!(
        Browser::new(server)
            .get(&format!("/posts/{slug}"))
            .await
            .status,
        404
    );
    assert_eq!(
        b.post_form(&format!("/admin/posts/{id}/delete"), "/admin", "")
            .await
            .status,
        404
    );

    // logout kills the session: the old cookie is useless afterwards
    let old = b.session_id().unwrap();
    let out = b.post_form("/admin/logout", "/admin", "").await;
    assert_eq!((out.status, out.header("location")), (303, "/"));
    assert!(b.session_id().is_none(), "cookie cleared");
    let replay = server.get("/admin").cookie("sid", &old).send().await;
    assert_eq!(replay.header("location"), "/admin/login");
}

#[tokio::test]
async fn posts_are_persisted_to_the_data_file() {
    let file: PathBuf =
        std::env::temp_dir().join(format!("ullun-blog-{}.json", std::process::id()));
    std::fs::remove_file(&file).ok();
    let clock = Arc::new(AtomicU64::new(1_700_000_000));
    let make = |clock: &Arc<AtomicU64>| {
        let mut c = config(clock.clone());
        c.data_file = Some(file.clone());
        c
    };

    let first = Browser::new(spawn(blog::build(make(&clock))).await);
    first.login().await;
    let slug = slug_from(&publish(&first, "Survives", "restart", "x").await);

    let second_server = spawn(blog::build(make(&clock))).await;
    let reader = Browser::new(second_server);
    assert!(reader
        .get(&format!("/posts/{slug}"))
        .await
        .text()
        .contains("restart"));
    // ids continue where they left off
    let admin = Browser::new(second_server);
    admin.login().await;
    let next = slug_from(&publish(&admin, "Next one", "t", "").await);
    assert_eq!(next, "next-one");
    assert!(!file.with_extension("tmp").exists());

    std::fs::write(&file, b"not json").unwrap();
    assert_eq!(
        Browser::new(spawn(blog::build(make(&clock))).await)
            .get("/")
            .await
            .status,
        200
    );
    std::fs::remove_file(&file).ok();
}

#[tokio::test]
async fn secure_deployments_get_prefixed_cookies_and_hsts() {
    let clock = Arc::new(AtomicU64::new(1));
    let mut cfg = config(clock);
    cfg.secure_cookies = true;
    let server = spawn(blog::build(cfg)).await;
    let login_page = server.get("/admin/login").send().await;
    let cookie = login_page
        .set_cookie("__Host-sid")
        .expect("prefixed cookie");
    assert!(
        cookie.contains("Secure")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Lax")
            && cookie.contains("Path=/")
    );
    assert!(login_page
        .header("strict-transport-security")
        .starts_with("max-age="));
}
