//! File drive: application logic, shared by `main.rs` and the tests

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ullun::files::{content_type_for, serve_file, FileOptions};
use ullun::multipart::{MultipartLimits, Part};
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::security::{constant_time_eq, random_alphanumeric, random_token};
use ullun::validate::Validator;

/// Types a browser may render inline; everything else is offered as a download
const INLINE_SAFE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "application/pdf",
    "text/plain; charset=utf-8",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "video/mp4",
    "video/webm",
];
const MAX_SHARE_TTL: u64 = 30 * 24 * 3600;

#[derive(Clone)]
pub struct UserConfig {
    pub name: String,
    pub api_key: String,
    pub quota_bytes: u64,
}

pub struct Config {
    pub users: Vec<UserConfig>,
    pub data_dir: PathBuf,
    pub base_url: String,
    pub max_file_bytes: u64,
    pub max_files_per_upload: usize,
    /// Uploads allowed per window and user
    pub upload_rate_limit: (u32, Duration),
    /// How often expired shares are removed
    pub cleanup_every: Duration,
    /// Seconds since the Unix epoch; replaced in tests
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Config {
    pub fn from_env() -> std::result::Result<Self, String> {
        let dev = std::env::var("DEV").is_ok_and(|v| v == "1");
        let spec = match std::env::var("DRIVE_USERS") {
            Ok(s) => s,
            Err(_) if dev => {
                "alice:alice-demo-key-0123456789:50,bob:bob-demo-key-0123456789:50".into()
            }
            Err(_) => return Err("DRIVE_USERS is required (name:key:quota-mb,...) or DEV=1".into()),
        };
        let mut users = Vec::new();
        for entry in spec.split(',').filter(|e| !e.trim().is_empty()) {
            let parts: Vec<&str> = entry.trim().split(':').collect();
            let [name, key, mb] = parts[..] else {
                return Err(format!(
                    "bad DRIVE_USERS entry {entry:?}, expected name:key:quota-mb"
                ));
            };
            if key.len() < 16 {
                return Err(format!("API key of {name} must be at least 16 characters"));
            }
            let mb: u64 = mb.parse().map_err(|_| format!("bad quota for {name}"))?;
            users.push(UserConfig {
                name: name.into(),
                api_key: key.into(),
                quota_bytes: mb << 20,
            });
        }
        if users.is_empty() {
            return Err("no users configured".into());
        }
        Ok(Self {
            users,
            data_dir: std::env::var("DATA_DIR")
                .unwrap_or_else(|_| "drive-data".into())
                .into(),
            base_url: String::new(),
            max_file_bytes: 25 << 20,
            max_files_per_upload: 10,
            upload_rate_limit: (30, Duration::from_secs(60)),
            cleanup_every: Duration::from_secs(60),
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

// ---- model ----

#[derive(Clone, Serialize, Deserialize)]
struct StoredFile {
    id: String,
    owner: String,
    name: String,
    size: u64,
    content_type: String,
    description: Option<String>,
    created_at: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Share {
    file_id: String,
    expires_at: u64,
    max_downloads: Option<u32>,
    downloads: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    files: HashMap<String, StoredFile>,
    shares: HashMap<String, Share>,
}

struct Store {
    index: Mutex<Index>,
    users: Vec<UserConfig>,
    blobs: PathBuf,
    index_file: PathBuf,
    base_url: String,
    max_file_bytes: u64,
    max_files_per_upload: usize,
    cleanup_every: Duration,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

/// The authenticated user, put into the request by `authenticate`
#[derive(Clone)]
struct Caller(UserConfig);

impl Store {
    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn lock(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.blobs.join(id)
    }

    fn file_view(&self, f: &StoredFile) -> Value {
        json!({
            "id": f.id,
            "name": f.name,
            "size": f.size,
            "content_type": f.content_type,
            "description": f.description,
            "created_at": f.created_at,
            "download_url": format!("{}/api/files/{}/content", self.base_url, f.id),
        })
    }

    fn used_by(index: &Index, owner: &str) -> u64 {
        index
            .files
            .values()
            .filter(|f| f.owner == owner)
            .map(|f| f.size)
            .sum()
    }

    /// Write the index atomically
    async fn persist(&self) {
        let json = {
            let index = self.lock();
            serde_json::to_vec_pretty(&*index)
        };
        let Ok(json) = json else { return };
        let tmp = self.index_file.with_extension("tmp");
        if tokio::fs::write(&tmp, json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &self.index_file).await;
        }
    }

    async fn load(config: &Config) -> std::io::Result<Self> {
        let blobs = config.data_dir.join("blobs");
        tokio::fs::create_dir_all(&blobs).await?;
        let index_file = config.data_dir.join("index.json");
        let mut index: Index = tokio::fs::read(&index_file)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // forget entries whose blob vanished, and shares of forgotten files
        index.files.retain(|id, _| blobs.join(id).is_file());
        let ids: Vec<String> = index.files.keys().cloned().collect();
        index.shares.retain(|_, s| ids.contains(&s.file_id));
        Ok(Self {
            index: Mutex::new(index),
            users: config.users.clone(),
            blobs,
            index_file,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            max_file_bytes: config.max_file_bytes,
            max_files_per_upload: config.max_files_per_upload,
            cleanup_every: config.cleanup_every,
            clock: config.clock.clone(),
        })
    }
}

fn store(req: &Request) -> Result<&Store> {
    req.state::<Store>()
}

fn caller(req: &Request) -> Result<&UserConfig> {
    req.extensions
        .get::<Caller>()
        .map(|c| &c.0)
        .ok_or_else(|| Error::unauthorized("not authenticated"))
}

// ---- middleware ----

async fn authenticate(mut req: Request, next: Next) -> Result<Response> {
    let given = req
        .headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let user = {
        let st = store(&req)?;
        // look at every key so the matching position does not leak
        st.users.iter().fold(None, |found, u| {
            let hit = constant_time_eq(u.api_key.as_bytes(), given.as_bytes());
            if hit {
                Some(u.clone())
            } else {
                found
            }
        })
    };
    let Some(user) = user else {
        return Err(Error::unauthorized("missing or invalid x-api-key"));
    };
    req.extensions.insert(Caller(user));
    next.run(req).await
}

// ---- upload ----

async fn upload(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let user = caller(&req)?.clone();
    let form = req.multipart_with(&MultipartLimits {
        max_parts: st.max_files_per_upload + 8,
        ..Default::default()
    })?;

    let files: Vec<&Part> = form.files().collect();
    if files.is_empty() {
        return Err(Error::bad_request(
            "send at least one file in a multipart/form-data field",
        ));
    }
    if files.len() > st.max_files_per_upload {
        return Err(Error::custom(
            413,
            format!("at most {} files per upload", st.max_files_per_upload),
        ));
    }
    let description = form
        .text("description")
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());

    let mut v = Validator::new();
    if let Some(d) = &description {
        v.len_between("description", d, 1, 500);
    }
    for (i, f) in files.iter().enumerate() {
        v.check(!f.data.is_empty(), &format!("files[{i}]"), "is empty");
    }
    v.finish()?;
    if let Some(big) = files
        .iter()
        .find(|f| f.data.len() as u64 > st.max_file_bytes)
    {
        return Err(Error::custom(
            413,
            format!(
                "{} is larger than the {} byte limit per file",
                big.safe_filename().unwrap_or_else(|| "file".into()),
                st.max_file_bytes
            ),
        ));
    }

    // check the whole upload against the quota first: all files or none
    let incoming: u64 = files.iter().map(|f| f.data.len() as u64).sum();
    {
        let index = st.lock();
        let used = Store::used_by(&index, &user.name);
        if used + incoming > user.quota_bytes {
            return Err(Error::custom(
                507,
                format!(
                    "quota exceeded: {} of {} bytes used, upload needs {}",
                    used, user.quota_bytes, incoming
                ),
            ));
        }
    }

    // write blobs (temp file + rename), remember them to roll back on failure
    let mut written: Vec<(StoredFile, PathBuf)> = Vec::new();
    for part in files {
        let id = random_alphanumeric(16)?;
        let name = part.safe_filename().unwrap_or_else(|| "file".to_string());
        let path = st.blob_path(&id);
        let tmp = path.with_extension("part");
        let result = async {
            tokio::fs::write(&tmp, &part.data).await?;
            tokio::fs::rename(&tmp, &path).await
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            for (_, p) in &written {
                let _ = tokio::fs::remove_file(p).await;
            }
            return Err(Error::internal("could not store the file"));
        }
        written.push((
            StoredFile {
                id,
                owner: user.name.clone(),
                // the type comes from the sanitized name, not from what the client claims
                content_type: content_type_for(Path::new(&name)).to_string(),
                name,
                size: part.data.len() as u64,
                description: description.clone(),
                created_at: st.now(),
            },
            path,
        ));
    }

    // re-check under the lock: a parallel upload may have used the quota meanwhile
    // (the guard is released before anything is awaited)
    let stored: std::result::Result<Vec<Value>, Vec<PathBuf>> = {
        let mut index = st.lock();
        if Store::used_by(&index, &user.name) + incoming > user.quota_bytes {
            Err(written.iter().map(|(_, p)| p.clone()).collect())
        } else {
            Ok(written
                .into_iter()
                .map(|(f, _)| {
                    let view = st.file_view(&f);
                    index.files.insert(f.id.clone(), f);
                    view
                })
                .collect())
        }
    };
    let views = match stored {
        Ok(views) => views,
        Err(paths) => {
            for p in paths {
                let _ = tokio::fs::remove_file(p).await;
            }
            return Err(Error::custom(507, "quota exceeded"));
        }
    };
    st.persist().await;
    Ok(Response::json(json!({ "files": views })).with_status(http::StatusCode::CREATED))
}

// ---- management ----

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn list_files(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let user = caller(&req)?;
    let q: ListQuery = req.query.parse()?;
    let needle = q.q.map(|s| s.to_lowercase());
    let (limit, offset) = (q.limit.unwrap_or(20).clamp(1, 100), q.offset.unwrap_or(0));

    let index = st.lock();
    let mut mine: Vec<&StoredFile> = index
        .files
        .values()
        .filter(|f| f.owner == user.name)
        .filter(|f| {
            needle
                .as_ref()
                .is_none_or(|n| f.name.to_lowercase().contains(n))
        })
        .collect();
    mine.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    let total = mine.len();
    let items: Vec<Value> = mine
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|f| st.file_view(f))
        .collect();
    Ok(Response::json(
        json!({ "total": total, "limit": limit, "offset": offset, "items": items }),
    ))
}

fn own_file(index: &Index, user: &UserConfig, id: &str) -> Result<StoredFile> {
    index
        .files
        .get(id)
        .filter(|f| f.owner == user.name)
        .cloned()
        .ok_or_else(|| Error::not_found("no such file"))
}

async fn file_info(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let file = own_file(&st.lock(), caller(&req)?, req.params.get("id")?)?;
    Ok(Response::json(st.file_view(&file)))
}

async fn delete_file(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let id = req.params.get("id")?.to_string();
    {
        let mut index = st.lock();
        own_file(&index, caller(&req)?, &id)?;
        index.files.remove(&id);
        index.shares.retain(|_, s| s.file_id != id);
    }
    let _ = tokio::fs::remove_file(st.blob_path(&id)).await;
    st.persist().await;
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

async fn usage(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let user = caller(&req)?;
    let index = st.lock();
    let used = Store::used_by(&index, &user.name);
    Ok(Response::json(json!({
        "used_bytes": used,
        "quota_bytes": user.quota_bytes,
        "files": index.files.values().filter(|f| f.owner == user.name).count(),
    })))
}

/// Owner download: ranges and conditional requests included
async fn download_own(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let file = own_file(&st.lock(), caller(&req)?, req.params.get("id")?)?;
    deliver(&req, st, &file).await
}

/// Inline only for types that cannot run script; everything else is a download,
/// sandboxed, with sniffing disabled.
async fn deliver(req: &Request, st: &Store, file: &StoredFile) -> Result<Response> {
    let inline_ok =
        INLINE_SAFE.contains(&file.content_type.as_str()) && req.query.get("download").is_none();
    let mut options = FileOptions::new()
        .content_type(file.content_type.clone())
        .cache_control("private, no-cache");
    if !inline_ok {
        options = options.attachment(file.name.clone());
    }
    let res = serve_file(req, st.blob_path(&file.id), &options).await?;
    Ok(res
        .with_header("x-content-type-options", "nosniff")
        .with_header("content-security-policy", "default-src 'none'; sandbox")
        .with_header("referrer-policy", "no-referrer"))
}

// ---- sharing ----

#[derive(Deserialize)]
struct NewShare {
    ttl_secs: Option<u64>,
    max_downloads: Option<u32>,
}

async fn create_share(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let user = caller(&req)?;
    let id = req.params.get("id")?.to_string();
    let body: NewShare = if req.body.is_empty() {
        NewShare {
            ttl_secs: None,
            max_downloads: None,
        }
    } else {
        req.json()?
    };
    let ttl = body.ttl_secs.unwrap_or(24 * 3600);

    let mut v = Validator::new();
    v.range("ttl_secs", ttl, 1, MAX_SHARE_TTL);
    if let Some(m) = body.max_downloads {
        v.range("max_downloads", m, 1, 1_000_000);
    }
    v.finish()?;

    own_file(&st.lock(), user, &id)?;
    let token = random_token(24)?;
    let expires_at = st.now() + ttl;
    st.lock().shares.insert(
        token.clone(),
        Share {
            file_id: id,
            expires_at,
            max_downloads: body.max_downloads,
            downloads: 0,
        },
    );
    st.persist().await;
    Ok(Response::json(json!({
        "url": format!("{}/s/{}", st.base_url, token),
        "token": token,
        "expires_at": expires_at,
        "max_downloads": body.max_downloads,
    }))
    .with_status(http::StatusCode::CREATED))
}

async fn revoke_share(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let user = caller(&req)?;
    let token = req.params.get("token")?.to_string();
    {
        let mut index = st.lock();
        let share = index
            .shares
            .get(&token)
            .ok_or_else(|| Error::not_found("no such share"))?;
        // only the owner of the file may revoke
        own_file(&index, user, &share.file_id).map_err(|_| Error::not_found("no such share"))?;
        index.shares.remove(&token);
    }
    st.persist().await;
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

/// Public download by share token
async fn download_shared(req: Request) -> Result<Response> {
    let st = store(&req)?;
    let token = req.params.get("token")?.to_string();
    let now = st.now();

    // A resumed or probing request must not use up the download budget: only a
    // GET starting at byte 0 counts, and only if the share still has budget.
    let counts = req.method == http::Method::GET
        && !req.headers.contains_key(http::header::IF_NONE_MATCH)
        && !req.headers.contains_key(http::header::IF_MODIFIED_SINCE)
        && req
            .headers
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|r| r.trim().starts_with("bytes=0-"));

    let file = {
        let mut index = st.lock();
        let share = index
            .shares
            .get(&token)
            .cloned()
            .ok_or_else(|| Error::not_found("no such share"))?;
        if now >= share.expires_at || share.max_downloads.is_some_and(|m| share.downloads >= m) {
            index.shares.remove(&token);
            return Err(Error::custom(410, "this share link has expired"));
        }
        let file = index
            .files
            .get(&share.file_id)
            .cloned()
            .ok_or_else(|| Error::not_found("no such share"))?;
        if counts {
            if let Some(s) = index.shares.get_mut(&token) {
                s.downloads += 1;
            }
        }
        file
    };
    deliver(&req, st, &file).await
}

// ---- background cleanup ----

async fn cleanup(ctx: Context) {
    let Ok(st) = ctx.state::<Store>() else { return };
    while ctx.sleep(st.cleanup_every).await {
        let now = st.now();
        let removed = {
            let mut index = st.lock();
            let before = index.shares.len();
            index.shares.retain(|_, s| {
                now < s.expires_at && s.max_downloads.is_none_or(|m| s.downloads < m)
            });
            before - index.shares.len()
        };
        if removed > 0 {
            st.persist().await;
        }
    }
}

// ---- wiring ----

fn user_op(summary: &str) -> Operation {
    Operation::new(summary)
        .tag("files")
        .secured_by_api_key("x-api-key")
        .error_response(401, "Missing or invalid API key")
}

pub async fn build(config: Config) -> std::io::Result<App> {
    let store = Store::load(&config).await?;
    let (limit, window) = config.upload_rate_limit;
    // multipart framing adds a little to the raw file sizes
    let upload_body_limit = (config.max_file_bytes as usize + 64 * 1024)
        .saturating_mul(config.max_files_per_upload.max(1));

    Ok(App::new()
        .state(store)
        .max_body_size(Some(16 * 1024))
        .background(cleanup)
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(json!({ "status": "ok" })))
        })
        .get("/s/{token}", download_shared)
        .doc(
            "GET",
            "/s/{token}",
            Operation::new("Download through a share link (supports Range)")
                .tag("public")
                .response(200, "The file")
                .response(206, "Partial content")
                .error_response(404, "Unknown link")
                .error_response(410, "Link expired or used up"),
        )
        .group("/api", move |g| {
            g.middleware(authenticate)
                .get("/files", list_files)
                .doc(
                    "GET",
                    "/files",
                    user_op("List your files")
                        .query_param("q", schema::string(), "name contains", false)
                        .query_param("limit", schema::integer(), "1-100, default 20", false)
                        .query_param("offset", schema::integer(), "default 0", false),
                )
                .get("/usage", usage)
                .doc("GET", "/usage", user_op("Quota usage"))
                .get("/files/{id}", file_info)
                .doc("GET", "/files/{id}", user_op("File details").error_response(404, "No such file"))
                .delete("/files/{id}", delete_file)
                .doc(
                    "DELETE",
                    "/files/{id}",
                    user_op("Delete a file and its share links").error_response(404, "No such file"),
                )
                .get("/files/{id}/content", download_own)
                .doc(
                    "GET",
                    "/files/{id}/content",
                    user_op("Download your file (Range, ETag, Last-Modified supported)")
                        .response(200, "The file")
                        .response(206, "Partial content")
                        .response(304, "Not modified")
                        .error_response(404, "No such file"),
                )
                .post("/files/{id}/share", create_share)
                .doc(
                    "POST",
                    "/files/{id}/share",
                    user_op("Create a share link")
                        .request_json(schema::object(
                            [("ttl_secs", schema::integer()), ("max_downloads", schema::integer())],
                            [],
                        ))
                        .error_response(404, "No such file")
                        .error_response(422, "Invalid ttl or download limit"),
                )
                .delete("/shares/{token}", revoke_share)
                .doc("DELETE", "/shares/{token}", user_op("Revoke a share link").error_response(404, "No such share"))
        })
        // uploads need a bigger body than the rest of the API, and their own budget
        .group("/api/upload", move |g| {
            g.max_body_size(Some(upload_body_limit))
                .middleware(authenticate)
                .middleware(RateLimiter::new(limit, window).key_by(|req| {
                    req.headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned)
                }))
                .post("", upload)
                .doc(
                    "POST",
                    "",
                    user_op("Upload files (multipart/form-data, field names are free, optional text field `description`)")
                        .error_response(400, "No file in the request")
                        .error_response(413, "File too large or too many files")
                        .error_response(415, "Not multipart/form-data")
                        .error_response(429, "Too many uploads")
                        .error_response(507, "Quota exceeded"),
                )
        })
        .openapi(OpenApiInfo::new("File drive", "1.0.0").swagger_ui("/docs")))
}
