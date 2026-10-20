//! URL shortener: application logic, shared by `main.rs` and the tests

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::security::{constant_time_eq, random_alphanumeric};

const MAX_URL_LEN: usize = 2048;
const MAX_TTL_SECS: u64 = 31 * 24 * 3600;
const CODE_LEN: usize = 7;
/// Paths that must never become short codes
const RESERVED: &[&str] = &["api", "healthz", "openapi.json", "docs", "favicon.ico"];

pub struct Config {
    pub api_keys: Vec<String>,
    pub base_url: String,
    pub data_file: Option<PathBuf>,
    /// Link creations allowed per window and API key
    pub rate_limit: (u32, Duration),
    /// Seconds since the Unix epoch; replaced in tests
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Config {
    pub fn from_env(port: &str) -> Self {
        let api_keys = std::env::var("API_KEYS")
            .unwrap_or_else(|_| "demo-key".into())
            .split(',')
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        Self {
            api_keys,
            base_url: std::env::var("BASE_URL")
                .unwrap_or_else(|_| format!("http://localhost:{port}")),
            data_file: std::env::var("DATA_FILE").ok().map(PathBuf::from),
            rate_limit: (30, Duration::from_secs(60)),
            clock: Arc::new(system_now),
        }
    }
}

fn system_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Serialize, Deserialize)]
struct Link {
    code: String,
    url: String,
    created_at: u64,
    expires_at: Option<u64>,
    clicks: u64,
}

struct Store {
    links: Mutex<HashMap<String, Link>>,
    file: Option<PathBuf>,
    base_url: String,
    api_keys: Vec<String>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Store {
    fn load(config: &Config) -> Self {
        let links = config
            .data_file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<Link>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|l| (l.code.clone(), l))
            .collect();
        Self {
            links: Mutex::new(links),
            file: config.data_file.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_keys: config.api_keys.clone(),
            clock: config.clock.clone(),
        }
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the current links atomically (temp file + rename)
    async fn persist(&self) {
        let Some(file) = &self.file else { return };
        let snapshot: Vec<Link> = self.lock().values().cloned().collect();
        let Ok(json) = serde_json::to_vec_pretty(&snapshot) else {
            return;
        };
        let tmp = file.with_extension("tmp");
        if tokio::fs::write(&tmp, json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, file).await;
        }
    }

    fn is_expired(&self, link: &Link) -> bool {
        link.expires_at.is_some_and(|t| self.now() >= t)
    }

    fn view(&self, link: &Link) -> serde_json::Value {
        serde_json::json!({
            "code": link.code,
            "short_url": format!("{}/{}", self.base_url, link.code),
            "url": link.url,
            "created_at": link.created_at,
            "expires_at": link.expires_at,
            "clicks": link.clicks,
        })
    }
}

#[derive(Deserialize)]
struct NewLink {
    url: String,
    code: Option<String>,
    ttl_secs: Option<u64>,
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

fn validate_url(url: &str) -> Result<()> {
    if url.len() > MAX_URL_LEN {
        return Err(Error::bad_request(format!(
            "url longer than {MAX_URL_LEN} bytes"
        )));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(Error::bad_request(
            "url must not contain whitespace or control characters",
        ));
    }
    let uri: http::Uri = url
        .parse()
        .map_err(|_| Error::bad_request("url is not valid"))?;
    // only web URLs: javascript:, data: and friends would make the short link a trap
    match uri.scheme_str() {
        Some("http" | "https") if uri.host().is_some_and(|h| !h.is_empty()) => Ok(()),
        _ => Err(Error::bad_request("url must be an absolute http(s) URL")),
    }
}

fn validate_code(code: &str) -> Result<()> {
    let ok_chars = code
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !(3..=32).contains(&code.len()) || !ok_chars {
        return Err(Error::bad_request(
            "code must be 3-32 characters of A-Z, a-z, 0-9, '-' or '_'",
        ));
    }
    if RESERVED.contains(&code.to_ascii_lowercase().as_str()) {
        return Err(Error::bad_request("code is reserved"));
    }
    Ok(())
}

// ---- handlers ----

async fn create_link(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let new: NewLink = req.json()?;
    validate_url(&new.url)?;
    if let Some(code) = &new.code {
        validate_code(code)?;
    }
    let ttl = match new.ttl_secs {
        Some(0) => return Err(Error::bad_request("ttl_secs must be positive")),
        Some(t) if t > MAX_TTL_SECS => {
            return Err(Error::bad_request(format!(
                "ttl_secs must be at most {MAX_TTL_SECS}"
            )))
        }
        other => other,
    };

    let now = store.now();
    let link = {
        let mut links = store.lock();
        // expired links free their codes
        links.retain(|_, l| l.expires_at.is_none_or(|t| now < t));
        let code = match new.code {
            Some(code) if links.contains_key(&code) => {
                return Err(Error::custom(409, "code is already taken"))
            }
            Some(code) => code,
            None => loop {
                let candidate = random_alphanumeric(CODE_LEN)?;
                if !links.contains_key(&candidate) && !RESERVED.contains(&candidate.as_str()) {
                    break candidate;
                }
            },
        };
        let link = Link {
            code: code.clone(),
            url: new.url,
            created_at: now,
            expires_at: ttl.map(|t| now + t),
            clicks: 0,
        };
        links.insert(code, link.clone());
        link
    };
    store.persist().await;

    let location = format!("{}/api/links/{}", store.base_url, link.code);
    let mut res = Response::json(store.view(&link)).with_header("location", location);
    res.status = http::StatusCode::CREATED;
    Ok(res)
}

async fn list_links(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let q: ListQuery = req.query.parse()?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let offset = q.offset.unwrap_or(0);

    let mut links: Vec<Link> = store
        .lock()
        .values()
        .filter(|l| !store.is_expired(l))
        .cloned()
        .collect();
    links.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.code.cmp(&b.code)));
    let total = links.len();
    let items: Vec<_> = links
        .iter()
        .skip(offset)
        .take(limit)
        .map(|l| store.view(l))
        .collect();
    Ok(Response::json(serde_json::json!({
        "total": total, "limit": limit, "offset": offset, "items": items
    })))
}

async fn get_link(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let code = req.params.get("code")?;
    let links = store.lock();
    match links.get(code).filter(|l| !store.is_expired(l)) {
        Some(link) => Ok(Response::json(store.view(link))),
        None => Err(Error::not_found("no such link")),
    }
}

async fn delete_link(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let code = req.params.get("code")?;
    let removed = store.lock().remove(code).is_some();
    if !removed {
        return Err(Error::not_found("no such link"));
    }
    store.persist().await;
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

/// Public redirect. 302, not 301: browsers must keep asking so that deleting
/// or expiring a link takes effect, and so every click is counted.
async fn follow(req: Request) -> Result<Response> {
    let store = req.state::<Store>()?;
    let code = req.params.get("code")?;
    let mut links = store.lock();
    let Some(link) = links.get_mut(code) else {
        return Err(Error::not_found("no such link"));
    };
    if store.is_expired(link) {
        links.remove(code);
        return Err(Error::custom(410, "this link has expired"));
    }
    link.clicks += 1;
    Ok(Response::new(http::StatusCode::FOUND)
        .with_header("location", link.url.clone())
        .with_header("cache-control", "no-store")
        .with_header("referrer-policy", "no-referrer"))
}

/// `x-api-key` check for the management API
async fn require_api_key(req: Request, next: Next) -> Result<Response> {
    let store = req.state::<Store>()?;
    let given = req
        .headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // compare against every key so the match position does not leak
    let known = store.api_keys.iter().fold(false, |acc, k| {
        constant_time_eq(k.as_bytes(), given.as_bytes()) | acc
    });
    if !known {
        return Err(Error::unauthorized("missing or invalid x-api-key"));
    }
    next.run(req).await
}

fn api_key_of(req: &Request) -> Option<String> {
    req.headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn link_schema() -> serde_json::Value {
    schema::object(
        [
            ("code", schema::string()),
            ("short_url", schema::string()),
            ("url", schema::string()),
            ("created_at", schema::integer()),
            ("expires_at", schema::integer()),
            ("clicks", schema::integer()),
        ],
        ["code", "short_url", "url", "created_at", "clicks"],
    )
}

/// Build the service
pub fn build(config: Config) -> App {
    let (limit, window) = config.rate_limit;
    let store = Store::load(&config);

    App::new()
        .state(store)
        .max_body_size(Some(8 * 1024))
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(serde_json::json!({ "status": "ok" })))
        })
        .group("/api", move |g| {
            g.middleware(require_api_key)
                // after the key check, so unauthenticated calls cannot burn a key's budget
                .middleware(RateLimiter::new(limit, window).key_by(api_key_of))
                .post("/links", create_link)
                .doc(
                    "POST",
                    "/links",
                    Operation::new("Create a short link")
                        .tag("links")
                        .secured_by_api_key("x-api-key")
                        .request_json(schema::object(
                            [
                                ("url", schema::string()),
                                ("code", schema::string()),
                                ("ttl_secs", schema::integer()),
                            ],
                            ["url"],
                        ))
                        .response_json(201, "Created", link_schema())
                        .error_response(400, "Invalid url, code or ttl")
                        .error_response(401, "Missing or invalid API key")
                        .error_response(409, "Code already taken")
                        .error_response(429, "Rate limit exceeded"),
                )
                .get("/links", list_links)
                .doc(
                    "GET",
                    "/links",
                    Operation::new("List links, newest first")
                        .tag("links")
                        .secured_by_api_key("x-api-key")
                        .query_param("limit", schema::integer(), "1-100, default 20", false)
                        .query_param("offset", schema::integer(), "default 0", false),
                )
                .get("/links/{code}", get_link)
                .doc(
                    "GET",
                    "/links/{code}",
                    Operation::new("Link details and click count")
                        .tag("links")
                        .secured_by_api_key("x-api-key")
                        .response_json(200, "The link", link_schema())
                        .error_response(404, "No such link"),
                )
                .delete("/links/{code}", delete_link)
                .doc(
                    "DELETE",
                    "/links/{code}",
                    Operation::new("Delete a link")
                        .tag("links")
                        .secured_by_api_key("x-api-key")
                        .response(204, "Deleted")
                        .error_response(404, "No such link"),
                )
        })
        // last: a single-segment wildcard, static routes above win
        .get("/{code}", follow)
        .doc(
            "GET",
            "/{code}",
            Operation::new("Follow a short link")
                .tag("public")
                .response(302, "Redirect to the target")
                .error_response(404, "No such link")
                .error_response(410, "Link expired"),
        )
        .openapi(OpenApiInfo::new("URL shortener", "1.0.0").description(
            "Create and manage short links. Management calls need an x-api-key header.",
        ))
}
