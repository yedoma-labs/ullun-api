//! Blog: application logic, shared by `main.rs` and the tests

use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ullun::conditional::ETags;
use ullun::csrf::Csrf;
use ullun::files::http_date;
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::security::constant_time_eq;
use ullun::session::SessionLayer;
use ullun::validate::Validator;

const PAGE_SIZE: usize = 5;
const FEED_ITEMS: usize = 20;

pub struct Config {
    pub site_title: String,
    pub base_url: String,
    pub admin_user: String,
    /// argon2 PHC string
    pub admin_password_hash: String,
    pub data_file: Option<PathBuf>,
    /// `Secure` + `__Host-` session cookie (needs HTTPS)
    pub secure_cookies: bool,
    pub dev: bool,
    pub login_attempts: (u32, Duration),
    /// Cheap argon2 settings for tests
    pub fast_hashing: bool,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

fn hasher(fast: bool) -> Argon2<'static> {
    let params = if fast {
        Params::new(8, 1, 1, None).expect("valid params")
    } else {
        Params::default()
    };
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// PHC hash of `password` with the production parameters
pub fn hash_password(password: &str) -> String {
    hash_with(&hasher(false), password)
}

fn hash_with(hasher: &Argon2<'_>, password: &str) -> String {
    hasher
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .expect("argon2 hashing")
}

impl Config {
    pub fn from_env(port: &str) -> std::result::Result<Self, String> {
        let dev = std::env::var("DEV").is_ok_and(|v| v == "1");
        let hash =
            match std::env::var("ADMIN_PASSWORD_HASH") {
                Ok(h) => {
                    PasswordHash::new(&h)
                        .map_err(|_| "ADMIN_PASSWORD_HASH is not a valid argon2 PHC string")?;
                    h
                }
                Err(_) if dev => hash_password("dev-password"),
                Err(_) => return Err(
                    "ADMIN_PASSWORD_HASH is required (create one with `hash-password`), or DEV=1"
                        .into(),
                ),
            };
        Ok(Self {
            site_title: std::env::var("SITE_TITLE").unwrap_or_else(|_| "My blog".into()),
            base_url: std::env::var("BASE_URL")
                .unwrap_or_else(|_| format!("http://localhost:{port}")),
            admin_user: std::env::var("ADMIN_USER").unwrap_or_else(|_| "admin".into()),
            admin_password_hash: hash,
            data_file: std::env::var("DATA_FILE").ok().map(PathBuf::from),
            secure_cookies: !dev,
            dev,
            login_attempts: (5, Duration::from_secs(60)),
            fast_hashing: false,
            clock: Arc::new(system_now),
        })
    }
}

fn system_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- Markdown ----

/// Markdown to HTML that cannot carry script: raw HTML is shown as text and
/// only http(s), mailto and relative links/images are kept.
pub mod markdown {
    use pulldown_cmark::{html, Event, Options, Parser, Tag};

    fn safe_url(url: &str) -> bool {
        let lower = url.trim().to_ascii_lowercase();
        // browsers drop tabs/newlines inside URLs and treat `\` like `/`, which
        // turns innocent-looking strings into schemes or protocol-relative URLs
        if lower.chars().any(|c| c.is_control()) || lower.contains('\\') {
            return false;
        }
        // a scheme is whatever precedes the first ':' before any '/', '?' or '#'
        match lower.find([':', '/', '?', '#']) {
            Some(i) if lower.as_bytes()[i] == b':' => {
                matches!(&lower[..i], "http" | "https" | "mailto")
            }
            _ => !lower.starts_with("//"),
        }
    }

    fn clean(url: pulldown_cmark::CowStr<'_>) -> pulldown_cmark::CowStr<'_> {
        if safe_url(&url) {
            url
        } else {
            "#".into()
        }
    }

    pub fn render(source: &str) -> String {
        let parser = Parser::new_ext(
            source,
            Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
        )
        .map(|event| match event {
            Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Link {
                link_type,
                dest_url: clean(dest_url),
                title,
                id,
            }),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Image {
                link_type,
                dest_url: clean(dest_url),
                title,
                id,
            }),
            other => other,
        });
        let mut out = String::new();
        html::push_html(&mut out, parser);
        out
    }

    /// Plain text of the first characters, for listings and feeds
    pub fn excerpt(source: &str, max_chars: usize) -> String {
        let mut text = String::new();
        for event in Parser::new(source) {
            match event {
                Event::Text(t) | Event::Code(t) => {
                    text.push_str(&t);
                    text.push(' ');
                }
                Event::SoftBreak | Event::HardBreak | Event::End(_) => text.push(' '),
                _ => {}
            }
            if text.chars().count() > max_chars * 2 {
                break;
            }
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() <= max_chars {
            text
        } else {
            let cut: String = text.chars().take(max_chars).collect();
            format!("{}…", cut.trim_end())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn renders_markdown_and_escapes_html() {
            let html = render("# Title\n\nSome **bold** and `code`.\n\n<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>");
            assert!(html.contains("<h1>Title</h1>") && html.contains("<strong>bold</strong>"));
            assert!(
                !html.contains("<script>") && !html.contains("<img"),
                "{html}"
            );
            assert!(html.contains("&lt;script&gt;"));
        }

        #[test]
        fn dangerous_link_targets_are_neutralised() {
            for evil in [
                "javascript:alert(1)",
                "JaVaScRiPt:alert(1)",
                " javascript:alert(1)",
                "data:text/html,<b>",
                "vbscript:x",
                "file:///etc/passwd",
                "//evil.example/x",
            ] {
                let html = render(&format!("[click]({evil}) ![img]({evil})"));
                assert!(html.contains("href=\"#\""), "{evil}: {html}");
                assert!(html.contains("src=\"#\""), "{evil}: {html}");
            }
            for ok in [
                "https://example.com/a?b=c",
                "http://example.com",
                "mailto:a@example.com",
                "/relative/path",
                "#anchor",
                "page.html",
            ] {
                let html = render(&format!("[x]({ok})"));
                assert!(
                    html.contains(&format!("href=\"{}\"", ok.replace('&', "&amp;"))),
                    "{ok}: {html}"
                );
            }
        }

        #[test]
        fn url_check_handles_browser_quirks() {
            for evil in [
                "java\tscript:alert(1)",
                "java\nscript:alert(1)",
                "\u{1}javascript:x",
                "/\\evil.example",
                "\\\\evil.example",
                "//evil.example",
            ] {
                assert!(!safe_url(evil), "{evil:?}");
            }
            for ok in [
                "https://example.com",
                "/a/b",
                "../up",
                "#x",
                "?q=1",
                "mailto:x@y.z",
                "relative.html",
            ] {
                assert!(safe_url(ok), "{ok:?}");
            }
        }

        #[test]
        fn excerpts_are_plain_text_and_bounded() {
            assert_eq!(
                excerpt("# Hi\n\nSome **bold** text", 100),
                "Hi Some bold text"
            );
            let long = "word ".repeat(100);
            let e = excerpt(&long, 20);
            assert!(e.chars().count() <= 21 && e.ends_with('…'), "{e}");
            // inline HTML contributes only its text, never the tags
            assert_eq!(excerpt("<b>raw</b>", 50), "raw");
        }
    }
}

// ---- model ----

#[derive(Clone, Serialize, Deserialize)]
struct Post {
    id: u64,
    slug: String,
    title: String,
    body: String,
    tags: Vec<String>,
    published: bool,
    created_at: u64,
    updated_at: u64,
    published_at: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Posts {
    next_id: u64,
    items: Vec<Post>,
}

struct Blog {
    posts: Mutex<Posts>,
    templates: Templates,
    site_title: String,
    base_url: String,
    admin_user: String,
    admin_hash: String,
    dummy_hash: String,
    hasher: Argon2<'static>,
    data_file: Option<PathBuf>,
    secure: bool,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Blog {
    fn lock(&self) -> MutexGuard<'_, Posts> {
        self.posts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    async fn persist(&self) {
        let Some(file) = &self.data_file else { return };
        let json = serde_json::to_vec_pretty(&*self.lock());
        let Ok(json) = json else { return };
        let tmp = file.with_extension("tmp");
        if tokio::fs::write(&tmp, json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, file).await;
        }
    }
}

fn blog(req: &Request) -> Result<&Blog> {
    req.state::<Blog>()
}

fn is_admin(req: &Request) -> bool {
    req.session()
        .ok()
        .and_then(|s| s.get::<bool>("admin"))
        .unwrap_or(false)
}

/// `29 Feb 2000` from a unix timestamp
fn day(secs: u64) -> String {
    let date = http_date(secs); // "Tue, 29 Feb 2000 00:00:00 GMT"
    date.split(' ')
        .skip(1)
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(60).collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "post".into()
    } else {
        slug
    }
}

fn unique_slug(posts: &Posts, base: &str, except: Option<u64>) -> String {
    let taken = |s: &str| {
        posts
            .items
            .iter()
            .any(|p| p.slug == s && Some(p.id) != except)
    };
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|s| !taken(s))
        .expect("endless iterator")
}

fn parse_tags(raw: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for t in raw
        .split(',')
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
    {
        if !tags.contains(&t) {
            tags.push(t);
        }
    }
    tags
}

// ---- rendering ----

/// Base context every page shares
fn context(b: &Blog, req: &Request, title: &str) -> Result<Value> {
    let session = req.session()?;
    let admin = is_admin(req);
    Ok(json!({
        "site_title": b.site_title,
        "title": title,
        "admin": admin,
        "flashes": session.take_flash(),
        // only pages with forms need a token, and creating one starts a session:
        // plain readers must stay cookie-free (privacy, cacheability)
        "csrf": if admin { session.csrf_token()? } else { String::new() },
    }))
}

fn render_page(b: &Blog, template: &str, ctx: &Value) -> Result<String> {
    b.templates.render(template, ctx)
}

fn post_card(p: &Post) -> Value {
    json!({
        "title": p.title,
        "slug": p.slug,
        "date": day(p.published_at.unwrap_or(p.created_at)),
        "excerpt": markdown::excerpt(&p.body, 200),
        "tags": p.tags,
        "draft": !p.published,
    })
}

#[derive(Deserialize)]
struct ListQuery {
    page: Option<usize>,
    q: Option<String>,
}

fn listing(b: &Blog, req: &Request, heading: &str, tag: Option<&str>) -> Result<Response> {
    let q: ListQuery = req.query.parse()?;
    let page = q.page.unwrap_or(1).max(1);
    let needle =
        q.q.as_ref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());

    let posts = b.lock();
    let mut matching: Vec<&Post> = posts
        .items
        .iter()
        .filter(|p| p.published)
        .filter(|p| tag.is_none_or(|t| p.tags.iter().any(|x| x == t)))
        .filter(|p| {
            needle.as_ref().is_none_or(|n| {
                p.title.to_lowercase().contains(n) || p.body.to_lowercase().contains(n)
            })
        })
        .collect();
    matching.sort_by(|a, b| b.published_at.cmp(&a.published_at).then(b.id.cmp(&a.id)));
    let total = matching.len();
    let pages = total.div_ceil(PAGE_SIZE).max(1);
    let items: Vec<Value> = matching
        .into_iter()
        .skip((page - 1) * PAGE_SIZE)
        .take(PAGE_SIZE)
        .map(post_card)
        .collect();

    let base_path = match tag {
        Some(t) => format!("/tags/{t}"),
        None => "/".to_string(),
    };
    let query_suffix = needle
        .as_ref()
        .map(|n| format!("&q={}", urlencoding::encode(n)))
        .unwrap_or_default();
    let mut ctx = context(b, req, heading)?;
    ctx["posts"] = json!(items);
    ctx["heading"] = json!(heading);
    ctx["tag"] = json!(tag);
    ctx["query"] = json!(needle);
    ctx["total"] = json!(total);
    if page > 1 {
        ctx["prev"] = json!(format!("{base_path}?page={}{query_suffix}", page - 1));
    }
    if page < pages {
        ctx["next"] = json!(format!("{base_path}?page={}{query_suffix}", page + 1));
    }
    Ok(
        Response::html(render_page(b, "index.html", &ctx)?)
            .with_header("cache-control", "no-cache"),
    )
}

async fn index(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    listing(b, &req, &b.site_title, None)
}

async fn by_tag(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let tag = req.params.get("tag")?.to_lowercase();
    listing(b, &req, &format!("Posts tagged “{tag}”"), Some(&tag))
}

async fn show_post(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let slug = req.params.get("slug")?;
    let post = b
        .lock()
        .items
        .iter()
        .find(|p| p.slug == slug)
        .cloned()
        .filter(|p| p.published || is_admin(&req)) // drafts are previewable by the admin
        .ok_or_else(|| Error::not_found("This post does not exist"))?;
    let mut ctx = context(b, &req, &post.title)?;
    ctx["post"] = json!({
        "id": post.id,
        "title": post.title,
        "date": day(post.published_at.unwrap_or(post.created_at)),
        "html": markdown::render(&post.body),
        "tags": post.tags,
        "draft": !post.published,
    });
    let res = Response::html(render_page(b, "post.html", &ctx)?);
    // drafts must not be cached by anything
    Ok(if post.published {
        res.with_header("cache-control", "no-cache")
    } else {
        res.with_header("cache-control", "no-store")
    })
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

async fn feed(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let posts = b.lock();
    let mut published: Vec<&Post> = posts.items.iter().filter(|p| p.published).collect();
    published.sort_by(|a, b| b.published_at.cmp(&a.published_at).then(b.id.cmp(&a.id)));
    let items: String = published
        .iter()
        .take(FEED_ITEMS)
        .map(|p| {
            let link = format!("{}/posts/{}", b.base_url, p.slug);
            format!(
                "<item><title>{}</title><link>{}</link><guid isPermaLink=\"true\">{}</guid><pubDate>{}</pubDate><description>{}</description></item>",
                xml(&p.title),
                xml(&link),
                xml(&link),
                http_date(p.published_at.unwrap_or(p.created_at)),
                xml(&markdown::excerpt(&p.body, 300)),
            )
        })
        .collect();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><rss version=\"2.0\"><channel><title>{}</title><link>{}</link><description>{}</description><lastBuildDate>{}</lastBuildDate>{}</channel></rss>",
        xml(&b.site_title),
        xml(&b.base_url),
        xml(&b.site_title),
        http_date(b.now()),
        items
    );
    Ok(Response::ok()
        .with_header("content-type", "application/rss+xml; charset=utf-8")
        .with_body(body))
}

async fn sitemap(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let posts = b.lock();
    let urls: String = std::iter::once(format!("<url><loc>{}/</loc></url>", xml(&b.base_url)))
        .chain(posts.items.iter().filter(|p| p.published).map(|p| {
            format!(
                "<url><loc>{}/posts/{}</loc><lastmod>{}</lastmod></url>",
                xml(&b.base_url),
                xml(&p.slug),
                iso_date(p.updated_at)
            )
        }))
        .collect();
    let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">{urls}</urlset>");
    Ok(Response::ok()
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(body))
}

/// `2000-02-29` from a unix timestamp
fn iso_date(secs: u64) -> String {
    let d = http_date(secs);
    let parts: Vec<&str> = d.split(' ').collect(); // [Tue, 29, Feb, 2000, ...]
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == parts[2])
    .map(|i| i + 1)
    .unwrap_or(1);
    format!("{}-{:02}-{}", parts[3], month, parts[1])
}

async fn robots(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    Ok(Response::text(format!(
        "User-agent: *\nDisallow: /admin\nSitemap: {}/sitemap.xml\n",
        b.base_url
    )))
}

const STYLE: &str = "body{font:17px/1.6 Georgia,serif;max-width:42rem;margin:0 auto;padding:1rem}\
nav{display:flex;gap:1rem;align-items:center;border-bottom:1px solid #ddd;margin-bottom:1.5rem}nav form{margin:0 0 0 auto}\
a{color:#0b5fff}h1,h2{line-height:1.2}.meta{color:#666;font-size:.9rem}.tag{background:#eef;padding:.1rem .5rem;border-radius:1rem;font-size:.85rem;margin-right:.3rem}\
.flash{background:#ffd;border:1px solid #cc9;padding:.5rem 1rem;margin:1rem 0}.error{color:#b00}.draft{color:#a60}\
input,textarea,button{font:inherit}input[type=text],input[type=password],textarea{width:100%;box-sizing:border-box}textarea{min-height:20rem;font-family:ui-monospace,monospace;font-size:14px}\
table{width:100%;border-collapse:collapse}td,th{border-bottom:1px solid #eee;padding:.4rem;text-align:left}pre{background:#f6f6f6;padding:1rem;overflow:auto}img{max-width:100%}\
.inline{display:inline}";

async fn style(_req: Request) -> Result<Response> {
    Ok(Response::ok()
        .with_header("content-type", "text/css; charset=utf-8")
        .with_header("cache-control", "public, max-age=3600")
        .with_body(STYLE))
}

// ---- admin ----

async fn login_page(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    if is_admin(&req) {
        return Ok(Response::redirect("/admin"));
    }
    let mut ctx = context(b, &req, "Sign in")?;
    ctx["csrf"] = json!(req.session()?.csrf_token()?);
    Ok(
        Response::html(render_page(b, "login.html", &ctx)?)
            .with_header("cache-control", "no-store"),
    )
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}

async fn login_submit(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let form: Login = req.form()?;

    // always spend the hashing time, whether or not the user name was right
    let user_ok = constant_time_eq(form.username.as_bytes(), b.admin_user.as_bytes());
    let hash = if user_ok {
        b.admin_hash.clone()
    } else {
        b.dummy_hash.clone()
    };
    let hasher = b.hasher.clone();
    let password = form.password;
    let password_ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|parsed| hasher.verify_password(password.as_bytes(), &parsed).is_ok())
            .unwrap_or(false)
    })
    .await
    .map_err(|_| Error::internal("hashing task failed"))?;

    let session = req.session()?;
    if user_ok && password_ok {
        session.renew(); // new session id on login: no fixation
        session.set("admin", true)?;
        session.flash("Welcome back.")?;
        Ok(Response::redirect("/admin"))
    } else {
        session.flash("Wrong user name or password.")?;
        Ok(Response::redirect("/admin/login"))
    }
}

async fn logout(req: Request) -> Result<Response> {
    req.session()?.destroy();
    Ok(Response::redirect("/"))
}

async fn require_admin(req: Request, next: Next) -> Result<Response> {
    if !is_admin(&req) {
        req.session()?.flash("Please sign in.")?;
        return Ok(Response::redirect("/admin/login"));
    }
    let res = next.run(req).await?;
    Ok(res.with_header("cache-control", "no-store"))
}

async fn dashboard(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let mut all: Vec<Post> = b.lock().items.clone();
    all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
    let rows: Vec<Value> = all
        .iter()
        .map(|p| json!({ "id": p.id, "title": p.title, "slug": p.slug, "published": p.published, "updated": day(p.updated_at) }))
        .collect();
    let mut ctx = context(b, &req, "Dashboard")?;
    ctx["rows"] = json!(rows);
    Ok(Response::html(render_page(b, "dashboard.html", &ctx)?))
}

fn editor_page(b: &Blog, req: &Request, post: Value, errors: Value) -> Result<String> {
    let mut ctx = context(
        b,
        req,
        if post["id"].is_null() {
            "New post"
        } else {
            "Edit post"
        },
    )?;
    ctx["post"] = post;
    ctx["errors"] = errors;
    render_page(b, "editor.html", &ctx)
}

async fn new_post_page(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    Ok(Response::html(editor_page(
        b,
        &req,
        json!({ "id": null, "title": "", "body": "", "tags": "", "published": false }),
        json!([]),
    )?))
}

async fn edit_post_page(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let id: u64 = req.params.parse("id")?;
    let post = b
        .lock()
        .items
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or_else(|| Error::not_found("no such post"))?;
    let view = json!({ "id": post.id, "title": post.title, "body": post.body, "tags": post.tags.join(", "), "published": post.published, "slug": post.slug });
    Ok(Response::html(editor_page(b, &req, view, json!([]))?))
}

#[derive(Deserialize)]
struct PostForm {
    title: String,
    body: String,
    tags: Option<String>,
    publish: Option<String>,
}

struct Checked {
    title: String,
    body: String,
    tags: Vec<String>,
}

fn check_post(form: &PostForm) -> std::result::Result<Checked, Vec<String>> {
    let title = form.title.trim().to_string();
    let tags = parse_tags(form.tags.as_deref().unwrap_or(""));
    let mut v = Validator::new();
    v.len_between("title", &title, 1, 120);
    v.len_between("body", form.body.trim(), 1, 50_000);
    v.check(tags.len() <= 8, "tags", "at most 8 tags");
    v.check(
        tags.iter().all(|t| {
            t.len() <= 24
                && t.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }),
        "tags",
        "tags may use a-z, 0-9 and '-' (24 characters at most)",
    );
    match v.finish() {
        Ok(()) => Ok(Checked {
            title,
            body: form.body.replace("\r\n", "\n"),
            tags,
        }),
        Err(e) => Err(e
            .fields()
            .iter()
            .map(|f| format!("{}: {}", f.field, f.message))
            .collect()),
    }
}

async fn save_post(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let id: Option<u64> = match req.params.get_opt("id") {
        Some(_) => Some(req.params.parse("id")?),
        None => None,
    };
    let form: PostForm = req.form()?;
    let checked = match check_post(&form) {
        Ok(c) => c,
        Err(errors) => {
            let view = json!({ "id": id, "title": form.title, "body": form.body, "tags": form.tags.clone().unwrap_or_default(), "published": form.publish.is_some() });
            let html = editor_page(b, &req, view, json!(errors))?;
            return Ok(Response::html(html).with_status(http::StatusCode::UNPROCESSABLE_ENTITY));
        }
    };
    let want_published = form.publish.is_some();
    let now = b.now();

    let slug = {
        let mut posts = b.lock();
        match id {
            Some(id) => {
                let idx = posts
                    .items
                    .iter()
                    .position(|p| p.id == id)
                    .ok_or_else(|| Error::not_found("no such post"))?;
                let slug = posts.items[idx].slug.clone();
                let post = &mut posts.items[idx];
                post.title = checked.title;
                post.body = checked.body;
                post.tags = checked.tags;
                post.updated_at = now;
                if want_published && !post.published {
                    post.published_at = Some(now);
                }
                post.published = want_published;
                slug
            }
            None => {
                posts.next_id += 1;
                let new_id = posts.next_id;
                let slug = unique_slug(&posts, &slugify(&checked.title), None);
                posts.items.push(Post {
                    id: new_id,
                    slug: slug.clone(),
                    title: checked.title,
                    body: checked.body,
                    tags: checked.tags,
                    published: want_published,
                    created_at: now,
                    updated_at: now,
                    published_at: want_published.then_some(now),
                });
                slug
            }
        }
    };
    b.persist().await;
    req.session()?.flash(if want_published {
        "Saved and published."
    } else {
        "Saved as draft."
    })?;
    Ok(Response::redirect(format!("/posts/{slug}")))
}

async fn toggle_publish(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let id: u64 = req.params.parse("id")?;
    let now = b.now();
    let message = {
        let mut posts = b.lock();
        let post = posts
            .items
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| Error::not_found("no such post"))?;
        post.published = !post.published;
        post.updated_at = now;
        if post.published && post.published_at.is_none() {
            post.published_at = Some(now);
        }
        if post.published {
            "Published."
        } else {
            "Moved back to drafts."
        }
    };
    b.persist().await;
    req.session()?.flash(message)?;
    Ok(Response::redirect("/admin"))
}

async fn delete_post(req: Request) -> Result<Response> {
    let b = blog(&req)?;
    let id: u64 = req.params.parse("id")?;
    {
        let mut posts = b.lock();
        let before = posts.items.len();
        posts.items.retain(|p| p.id != id);
        if posts.items.len() == before {
            return Err(Error::not_found("no such post"));
        }
    }
    b.persist().await;
    req.session()?.flash("Post deleted.")?;
    Ok(Response::redirect("/admin"))
}

// ---- cross-cutting ----

/// Strict by design: no inline script or style, nothing framed, forms only to us
async fn security_headers(req: Request, next: Next) -> Result<Response> {
    let secure = blog(&req)?.secure;
    let res = match next.run(req).await {
        Ok(res) => res,
        Err(err) => Response::from_error(err),
    };
    let res = res
        .with_header(
            "content-security-policy",
            "default-src 'none'; style-src 'self'; img-src 'self' https: data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        )
        .with_header("x-content-type-options", "nosniff")
        .with_header("referrer-policy", "strict-origin-when-cross-origin")
        .with_header("x-frame-options", "DENY");
    Ok(if secure {
        res.with_header("strict-transport-security", "max-age=31536000")
    } else {
        res
    })
}

/// Browsers get a readable error page, API clients keep the JSON error
///
/// Inner middleware (ETags, the rate limiter) may already have turned an error
/// into a JSON response, so those are converted as well.
async fn html_errors(req: Request, next: Next) -> Result<Response> {
    let wants_html = req
        .headers
        .get(http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    match next.run(req).await {
        Err(err) if wants_html => Ok(error_page(err)),
        Ok(res) if wants_html && res.status.as_u16() >= 400 && is_json(&res) => {
            let message = serde_json::from_slice::<Value>(&res.body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "Something went wrong".into());
            let mut page = error_page(Error::custom(res.status.as_u16(), message));
            // keep what the original response wanted to say besides its body
            for name in ["retry-after", "allow"] {
                if let Some(v) = res.headers.get(name) {
                    page.headers
                        .insert(http::HeaderName::from_static(name), v.clone());
                }
            }
            Ok(page)
        }
        other => other,
    }
}

fn is_json(res: &Response) -> bool {
    res.headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"))
}

fn error_page(err: Error) -> Response {
    let status = err.status();
    let message = err.message();
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{code}</title><link rel=\"stylesheet\" href=\"/style.css\"><h1>{code}</h1><p>{escaped}</p><p><a href=\"/\">Back to the blog</a></p>",
        code = status.as_u16()
    );
    Response::html(html).with_status(status)
}

fn templates() -> Templates {
    let mut t = Templates::new();
    let mut add = |name: &str, src: &str| {
        t.add(name, src)
            .unwrap_or_else(|e| panic!("template {name}: {}", e.message()));
    };
    add(
        "top.html",
        r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{title}}</title>
<link rel="stylesheet" href="/style.css">
<link rel="alternate" type="application/rss+xml" title="{{site_title}}" href="/feed.xml">
<body>
<nav><a href="/"><strong>{{site_title}}</strong></a><a href="/feed.xml">RSS</a>
{{#if admin}}<a href="/admin">Dashboard</a>
<form method="post" action="/admin/logout"><input type="hidden" name="csrf_token" value="{{csrf}}"><button>Sign out</button></form>
{{else}}<a href="/admin">Admin</a>{{/if}}</nav>
{{#each flashes}}<p class="flash">{{this}}</p>{{/each}}
"#,
    );
    add("bottom.html", "</body></html>\n");
    add(
        "index.html",
        r#"{{> top.html}}
<h1>{{heading}}</h1>
<form method="get" action="/"><input type="text" name="q" value="{{query}}" placeholder="Search"></form>
{{#each posts}}
<article>
<h2><a href="/posts/{{slug}}">{{title}}</a></h2>
<p class="meta">{{date}}{{#if draft}} · <span class="draft">draft</span>{{/if}}{{#each tags}} <a class="tag" href="/tags/{{this}}">{{this}}</a>{{/each}}</p>
<p>{{excerpt}}</p>
</article>
{{else}}<p>No posts{{#if query}} for “{{query}}”{{/if}}.</p>{{/each}}
<p>{{#if prev}}<a href="{{prev}}">← newer</a>{{/if}} {{#if next}}<a href="{{next}}">older →</a>{{/if}}</p>
{{> bottom.html}}"#,
    );
    add(
        "post.html",
        r#"{{> top.html}}
<article>
<h1>{{post.title}}</h1>
<p class="meta">{{post.date}}{{#if post.draft}} · <span class="draft">draft (only you can see this)</span>{{/if}}{{#each post.tags}} <a class="tag" href="/tags/{{this}}">{{this}}</a>{{/each}}</p>
{{{post.html}}}
</article>
{{#if admin}}<p><a href="/admin/posts/{{post.id}}/edit">Edit</a></p>{{/if}}
{{> bottom.html}}"#,
    );
    add(
        "login.html",
        r#"{{> top.html}}
<h1>Sign in</h1>
<form method="post" action="/admin/login">
<input type="hidden" name="csrf_token" value="{{csrf}}">
<p><label>User <input type="text" name="username" autocomplete="username" required></label></p>
<p><label>Password <input type="password" name="password" autocomplete="current-password" required></label></p>
<p><button>Sign in</button></p>
</form>
{{> bottom.html}}"#,
    );
    add(
        "dashboard.html",
        r#"{{> top.html}}
<h1>Dashboard</h1>
<p><a href="/admin/posts/new">New post</a></p>
<table><tr><th>Title</th><th>Status</th><th>Updated</th><th></th></tr>
{{#each rows}}<tr><td><a href="/posts/{{slug}}">{{title}}</a></td><td>{{#if published}}published{{else}}<span class="draft">draft</span>{{/if}}</td><td>{{updated}}</td>
<td><a href="/admin/posts/{{id}}/edit">edit</a>
<form class="inline" method="post" action="/admin/posts/{{id}}/publish"><input type="hidden" name="csrf_token" value="{{csrf}}"><button>{{#if published}}unpublish{{else}}publish{{/if}}</button></form>
<form class="inline" method="post" action="/admin/posts/{{id}}/delete"><input type="hidden" name="csrf_token" value="{{csrf}}"><button>delete</button></form></td></tr>
{{else}}<tr><td colspan="4">Nothing here yet.</td></tr>{{/each}}</table>
{{> bottom.html}}"#,
    );
    add(
        "editor.html",
        r#"{{> top.html}}
<h1>{{title}}</h1>
{{#each errors}}<p class="error">{{this}}</p>{{/each}}
<form method="post" action="{{#if post.id}}/admin/posts/{{post.id}}{{else}}/admin/posts{{/if}}">
<input type="hidden" name="csrf_token" value="{{csrf}}">
<p><label>Title <input type="text" name="title" value="{{post.title}}" required maxlength="120"></label></p>
<p><label>Tags (comma separated) <input type="text" name="tags" value="{{post.tags}}"></label></p>
<p><label>Text (Markdown)<textarea name="body" required>{{post.body}}</textarea></label></p>
<p><label><input type="checkbox" name="publish" value="1"{{#if post.published}} checked{{/if}}> Published</label> <button>Save</button></p>
</form>
{{> bottom.html}}"#,
    );
    t
}

fn load(config: &Config) -> Posts {
    config
        .data_file
        .as_ref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn build(config: Config) -> App {
    let hasher = hasher(config.fast_hashing);
    let dummy_hash = hash_with(&hasher, "dummy password for timing");
    let blog = Blog {
        posts: Mutex::new(load(&config)),
        templates: templates(),
        site_title: config.site_title,
        base_url: config.base_url.trim_end_matches('/').to_string(),
        admin_user: config.admin_user,
        admin_hash: config.admin_password_hash,
        dummy_hash,
        hasher,
        data_file: config.data_file,
        secure: config.secure_cookies,
        clock: config.clock.clone(),
    };
    let (attempts, window) = config.login_attempts;
    let clock = config.clock;

    let sessions = {
        let layer = SessionLayer::new().clock(move || clock());
        if config.secure_cookies {
            layer
        } else {
            layer.insecure_cookies()
        }
    };

    App::new()
        .state(blog)
        .max_body_size(Some(128 * 1024))
        .middleware(security_headers)
        .middleware(html_errors)
        .middleware(sessions)
        .middleware(Csrf::new())
        .get("/style.css", style)
        .get("/robots.txt", robots)
        .group("", |g| {
            g.middleware(ETags)
                .get("/", index)
                .get("/tags/{tag}", by_tag)
                .get("/posts/{slug}", show_post)
                .get("/feed.xml", feed)
                .get("/sitemap.xml", sitemap)
        })
        .get("/admin/login", login_page)
        // only submissions count against the budget, loading the form does not
        .group("/admin/login", move |g| {
            g.middleware(RateLimiter::new(attempts, window))
                .post("", login_submit)
        })
        .group("/admin", |g| {
            g.middleware(require_admin)
                .post("/logout", logout)
                .get("", dashboard)
                .get("/posts/new", new_post_page)
                .post("/posts", save_post)
                .get("/posts/{id}/edit", edit_post_page)
                .post("/posts/{id}", save_post)
                .post("/posts/{id}/publish", toggle_publish)
                .post("/posts/{id}/delete", delete_post)
        })
}
