//! Pastebin: application logic, shared by `main.rs` and the tests

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::security::{constant_time_eq, random_alphanumeric, random_token};

const MAX_CONTENT_BYTES: usize = 64 * 1024;
const MAX_TITLE_CHARS: usize = 80;
const DEFAULT_TTL: u64 = 24 * 3600;
const MAX_TTL: u64 = 7 * 24 * 3600;

pub struct Config {
    pub base_url: String,
    /// Pastes accepted at the same time; creating more answers 507
    pub max_pastes: usize,
    /// Creations allowed per window and client IP
    pub rate_limit: (u32, Duration),
    /// Seconds since the Unix epoch; replaced in tests
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Config {
    pub fn from_env(port: &str) -> Self {
        Self {
            base_url: std::env::var("BASE_URL")
                .unwrap_or_else(|_| format!("http://localhost:{port}")),
            max_pastes: 10_000,
            rate_limit: (20, Duration::from_secs(60)),
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        }
    }
}

struct Paste {
    title: String,
    content: String,
    delete_token: String,
    expires_at: u64,
    burn_after_read: bool,
}

struct Store {
    pastes: Mutex<HashMap<String, Paste>>,
    templates: Templates,
    base_url: String,
    max_pastes: usize,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Store {
    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Paste>> {
        self.pastes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fetch a live paste; burn-after-read pastes are removed by the fetch
    fn take_view(&self, id: &str) -> Result<(String, String)> {
        let now = self.now();
        let mut pastes = self.lock();
        let live = match pastes.get(id) {
            Some(p) if p.expires_at > now => p,
            Some(_) => {
                pastes.remove(id);
                return Err(Error::not_found("no such paste"));
            }
            None => return Err(Error::not_found("no such paste")),
        };
        let view = (live.title.clone(), live.content.clone());
        if live.burn_after_read {
            pastes.remove(id);
        }
        Ok(view)
    }
}

#[derive(Deserialize)]
struct NewPaste {
    content: String,
    title: Option<String>,
    ttl_secs: Option<u64>,
    #[serde(default)]
    burn_after_read: bool,
}

/// What both entry points (JSON API and HTML form) turn into
struct Draft {
    content: String,
    title: String,
    ttl: u64,
    burn_after_read: bool,
}

impl Draft {
    fn validate(raw: NewPaste) -> Result<Draft> {
        if raw.content.trim().is_empty() {
            return Err(Error::bad_request("content must not be empty"));
        }
        if raw.content.len() > MAX_CONTENT_BYTES {
            return Err(Error::custom(
                413,
                format!("content larger than {MAX_CONTENT_BYTES} bytes"),
            ));
        }
        let title = raw.title.unwrap_or_default().trim().to_string();
        if title.chars().count() > MAX_TITLE_CHARS || title.chars().any(char::is_control) {
            return Err(Error::bad_request(format!(
                "title must be at most {MAX_TITLE_CHARS} characters without control characters"
            )));
        }
        let ttl = raw.ttl_secs.unwrap_or(DEFAULT_TTL);
        if ttl == 0 || ttl > MAX_TTL {
            return Err(Error::bad_request(format!(
                "ttl_secs must be 1..={MAX_TTL}"
            )));
        }
        Ok(Draft {
            content: raw.content,
            title,
            ttl,
            burn_after_read: raw.burn_after_read,
        })
    }
}

#[derive(Serialize)]
struct Created {
    id: String,
    url: String,
    raw_url: String,
    delete_token: String,
    expires_at: u64,
}

fn store_paste(store: &Store, draft: Draft) -> Result<Created> {
    let now = store.now();
    let mut pastes = store.lock();
    pastes.retain(|_, p| p.expires_at > now);
    if pastes.len() >= store.max_pastes {
        return Err(Error::custom(507, "storage is full, try again later"));
    }
    let id = loop {
        let candidate = random_alphanumeric(10)?;
        if !pastes.contains_key(&candidate) {
            break candidate;
        }
    };
    let delete_token = random_token(16)?;
    let expires_at = now + draft.ttl;
    pastes.insert(
        id.clone(),
        Paste {
            title: draft.title,
            content: draft.content,
            delete_token: delete_token.clone(),
            expires_at,
            burn_after_read: draft.burn_after_read,
        },
    );
    Ok(Created {
        url: format!("{}/p/{}", store.base_url, id),
        raw_url: format!("{}/raw/{}", store.base_url, id),
        id,
        delete_token,
        expires_at,
    })
}

// ---- handlers ----

/// JSON (`application/json`) or the raw text itself (`text/plain`)
async fn create_paste(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let content_type = req
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    let raw = match content_type.as_str() {
        "application/json" => req.json::<NewPaste>()?,
        "text/plain" => NewPaste {
            content: String::from_utf8(req.body.to_vec())
                .map_err(|_| Error::bad_request("body is not valid UTF-8"))?,
            title: None,
            ttl_secs: None,
            burn_after_read: false,
        },
        _ => return Err(Error::custom(415, "send application/json or text/plain")),
    };
    let created = store_paste(store, Draft::validate(raw)?)?;
    let mut res = Response::json(&created).with_header("location", created.url.clone());
    res.status = http::StatusCode::CREATED;
    Ok(res)
}

#[derive(Deserialize)]
struct PasteForm {
    content: String,
    title: Option<String>,
    ttl_hours: Option<u64>,
    burn_after_read: Option<String>,
}

/// The HTML form posts here; success redirects to the paste (PRG)
async fn create_from_form(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let form: PasteForm = req.form()?;
    let draft = Draft::validate(NewPaste {
        content: form.content,
        title: form.title,
        ttl_secs: form.ttl_hours.map(|h| h.saturating_mul(3600)),
        burn_after_read: form.burn_after_read.is_some(),
    });
    match draft {
        Ok(draft) => {
            let created = store_paste(store, draft)?;
            // the delete token is shown on the paste page reached by this redirect
            Ok(Response::redirect(format!(
                "/p/{}?token={}",
                created.id, created.delete_token
            )))
        }
        Err(e) => Ok(Response::html(page(
            store,
            "New paste",
            &serde_json::json!({ "error": e.message() }),
        )?)
        .with_status(e.status())),
    }
}

fn page(store: &Store, title: &str, extra: &serde_json::Value) -> Result<String> {
    let mut ctx = serde_json::json!({ "title": title });
    if let (Some(base), Some(extra)) = (ctx.as_object_mut(), extra.as_object()) {
        base.extend(extra.clone());
    }
    store.templates.render("layout.html", &ctx)
}

fn html_headers(res: Response) -> Response {
    res.with_header(
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
    )
    .with_header("x-content-type-options", "nosniff")
    .with_header("referrer-policy", "no-referrer")
}

async fn index(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    Ok(html_headers(Response::html(page(
        store,
        "New paste",
        &serde_json::json!({}),
    )?)))
}

async fn view_html(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let id = req.params.get("id")?;
    let (title, content) = store.take_view(id)?;
    let token = req.query.get("token").map(str::to_string);
    let body = page(
        store,
        if title.is_empty() { "Paste" } else { &title },
        &serde_json::json!({
            "paste": { "id": id, "content": content, "token": token },
        }),
    )?;
    // page with a delete token must not end up in caches or history intermediaries
    Ok(html_headers(Response::html(body)).with_header("cache-control", "no-store"))
}

async fn view_raw(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let (_, content) = store.take_view(req.params.get("id")?)?;
    Ok(Response::ok()
        .with_header("content-type", "text/plain; charset=utf-8")
        .with_header("x-content-type-options", "nosniff")
        .with_header("content-security-policy", "default-src 'none'; sandbox")
        .with_header("cache-control", "no-store")
        .with_body(content))
}

async fn delete_paste(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let id = req.params.get("id")?;
    let given = req
        .headers
        .get("x-delete-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut pastes = store.lock();
    let Some(paste) = pastes.get(id) else {
        return Err(Error::not_found("no such paste"));
    };
    if !constant_time_eq(paste.delete_token.as_bytes(), given.as_bytes()) {
        return Err(Error::forbidden("missing or wrong x-delete-token"));
    }
    pastes.remove(id);
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

fn templates() -> Templates {
    let mut t = Templates::new();
    t.add(
        "layout.html",
        r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{title}}</title>
<style>
 body{font:16px/1.5 system-ui;max-width:48rem;margin:2rem auto;padding:0 1rem}
 textarea{width:100%;min-height:16rem;font:14px/1.4 ui-monospace,monospace}
 pre{background:#f5f5f5;padding:1rem;overflow:auto;white-space:pre-wrap;word-break:break-word}
 .error{color:#b00}.token{background:#ffd;padding:.5rem}
</style>
<body>
<h1><a href="/" style="color:inherit;text-decoration:none">Pastebin</a></h1>
{{#if error}}<p class="error">{{error}}</p>{{/if}}
{{#if paste}}
 <h2>{{title}}</h2>
 {{#if paste.token}}<p class="token">Delete token (only shown with this link): <code>{{paste.token}}</code></p>{{/if}}
 <pre>{{paste.content}}</pre>
 <p><a href="/raw/{{paste.id}}">raw</a></p>
{{else}}
 <form method="post" action="/new">
  <p><input name="title" maxlength="80" placeholder="Title (optional)" style="width:100%"></p>
  <p><textarea name="content" required placeholder="Paste text here"></textarea></p>
  <p>Expires after
   <select name="ttl_hours"><option value="1">1 hour</option><option value="24" selected>1 day</option><option value="168">7 days</option></select>
   <label><input type="checkbox" name="burn_after_read" value="1"> burn after reading</label>
   <button>Create</button></p>
 </form>
{{/if}}
</body></html>"#,
    )
    .expect("layout template parses");
    t
}

/// Build the service
pub fn build(config: Config) -> App {
    let store = Store {
        pastes: Mutex::new(HashMap::new()),
        templates: templates(),
        base_url: config.base_url.trim_end_matches('/').to_string(),
        max_pastes: config.max_pastes,
        clock: config.clock,
    };
    let (limit, window) = config.rate_limit;
    // one budget for creations through the API and through the form
    let creation_limit = Arc::new(RateLimiter::new(limit, window));
    let (api_limit, form_limit) = (creation_limit.clone(), creation_limit);

    App::new()
        .state(store)
        .max_body_size(Some(2 * MAX_CONTENT_BYTES))
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(serde_json::json!({ "status": "ok" })))
        })
        .get("/", index)
        .group("/new", move |g| {
            g.middleware(form_limit).post("", create_from_form)
        })
        .get("/p/{id}", view_html)
        .get("/raw/{id}", view_raw)
        .group("/pastes", move |g| {
            g.middleware(api_limit)
                .post("", create_paste)
                .delete("/{id}", delete_paste)
        })
}
