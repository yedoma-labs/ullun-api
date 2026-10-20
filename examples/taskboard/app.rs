//! Task board: application logic, shared by `main.rs` and the tests

use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use ullun::conditional::{require_if_match, ETags};
use ullun::jwt::{self, Claims, JwtAuth};
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::validate::Validator;

const STATUSES: [&str; 3] = ["open", "in_progress", "done"];

pub struct Config {
    pub jwt_secret: Vec<u8>,
    pub token_ttl_secs: u64,
    /// Cheap argon2 settings so tests do not spend seconds hashing
    pub fast_hashing: bool,
    /// Register/login attempts per window and client IP
    pub auth_rate_limit: (u32, Duration),
}

impl Config {
    pub fn from_env() -> std::result::Result<Self, String> {
        let dev = std::env::var("DEV").is_ok_and(|v| v == "1");
        let secret = match std::env::var("JWT_SECRET") {
            Ok(s) => s,
            Err(_) if dev => "dev-only-secret-dev-only-secret-xx".to_string(),
            Err(_) => return Err("JWT_SECRET is required (or DEV=1 for a demo secret)".into()),
        };
        if secret.len() < 32 {
            return Err("JWT_SECRET must be at least 32 bytes".into());
        }
        Ok(Self {
            jwt_secret: secret.into_bytes(),
            token_ttl_secs: 3600,
            fast_hashing: false,
            auth_rate_limit: (10, Duration::from_secs(60)),
        })
    }
}

// ---- data ----

struct User {
    id: u64,
    email: String,
    password_hash: String,
}

struct Project {
    id: u64,
    owner: u64,
    name: String,
}

#[derive(Clone)]
struct Task {
    id: u64,
    project_id: u64,
    title: String,
    description: Option<String>,
    status: &'static str,
    priority: u8,
    due: Option<String>,
    version: u64,
    created_seq: u64,
}

impl Task {
    fn etag(&self) -> String {
        format!("\"v{}\"", self.version)
    }

    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "project_id": self.project_id,
            "title": self.title,
            "description": self.description,
            "status": self.status,
            "priority": self.priority,
            "due": self.due,
            "version": self.version,
        })
    }
}

#[derive(Default)]
struct Data {
    next_id: u64,
    users: Vec<User>,
    projects: Vec<Project>,
    tasks: BTreeMap<u64, Task>,
}

impl Data {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Projects of other users do not exist as far as the caller can tell
    fn project_of(&self, user: u64, project_id: u64) -> Result<&Project> {
        self.projects
            .iter()
            .find(|p| p.id == project_id && p.owner == user)
            .ok_or_else(|| Error::not_found("no such project"))
    }

    fn task_of(&self, user: u64, task_id: u64) -> Result<&Task> {
        let task = self
            .tasks
            .get(&task_id)
            .ok_or_else(|| Error::not_found("no such task"))?;
        self.project_of(user, task.project_id)
            .map_err(|_| Error::not_found("no such task"))?;
        Ok(task)
    }
}

struct AppState {
    data: Mutex<Data>,
    secret: Vec<u8>,
    token_ttl: u64,
    hasher: Argon2<'static>,
    /// Verified against when the e-mail is unknown, so both failures cost the same
    dummy_hash: String,
}

impl AppState {
    fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn hash_password(hasher: &Argon2<'_>, password: &str) -> Result<String> {
    hasher
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|_| Error::internal("password hashing failed"))
}

fn verify_password(hasher: &Argon2<'_>, password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| hasher.verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

// ---- helpers ----

fn current_user(req: &Request) -> Result<u64> {
    req.extensions
        .get::<Claims>()
        .and_then(|c| c.0["sub"].as_u64())
        .ok_or_else(|| Error::unauthorized("invalid token subject"))
}

fn state(req: &Request) -> Result<&AppState> {
    req.state::<AppState>()
}

fn created(body: Value, location: String) -> Response {
    Response::json(body)
        .with_header("location", location)
        .with_status(http::StatusCode::CREATED)
}

/// `YYYY-MM-DD` with a real calendar date
fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<u32>().ok();
    let (Some(y), Some(m), Some(d)) = (num(0..4), num(5..7), num(8..10)) else {
        return false;
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d) && y >= 1
}

// ---- auth ----

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}

async fn register(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let creds: Credentials = req.json()?;
    let email = creds.email.trim().to_lowercase();

    let mut v = Validator::new();
    v.email("email", &email);
    v.len_between("password", &creds.password, 10, 128);
    v.finish()?;

    // hashing is CPU heavy: keep it off the async worker threads
    let hasher = st.hasher.clone();
    let password = creds.password;
    let hash = tokio::task::spawn_blocking(move || hash_password(&hasher, &password))
        .await
        .map_err(|_| Error::internal("hashing task failed"))??;

    let mut data = st.lock();
    if data.users.iter().any(|u| u.email == email) {
        return Err(Error::custom(
            409,
            "an account with this e-mail already exists",
        ));
    }
    let id = data.id();
    data.users.push(User {
        id,
        email: email.clone(),
        password_hash: hash,
    });
    Ok(created(
        json!({ "id": id, "email": email }),
        "/api/me".to_string(),
    ))
}

async fn login(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let creds: Credentials = req.json()?;
    let email = creds.email.trim().to_lowercase();

    let found = st
        .lock()
        .users
        .iter()
        .find(|u| u.email == email)
        .map(|u| (u.id, u.password_hash.clone()));
    let (hash, user_id) = match &found {
        Some((id, hash)) => (hash.clone(), Some(*id)),
        None => (st.dummy_hash.clone(), None),
    };

    let hasher = st.hasher.clone();
    let password = creds.password;
    let ok = tokio::task::spawn_blocking(move || verify_password(&hasher, &password, &hash))
        .await
        .map_err(|_| Error::internal("hashing task failed"))?;

    match (ok, user_id) {
        (true, Some(id)) => {
            let token = jwt::encode(
                &json!({ "sub": id, "exp": jwt::expires_in(st.token_ttl) }),
                &st.secret,
            )?;
            Ok(Response::json(json!({
                "token": token, "token_type": "Bearer", "expires_in": st.token_ttl
            })))
        }
        _ => Err(Error::unauthorized("invalid e-mail or password")),
    }
}

async fn me(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let id = current_user(&req)?;
    let data = st.lock();
    let user = data
        .users
        .iter()
        .find(|u| u.id == id)
        .ok_or_else(|| Error::unauthorized("account no longer exists"))?;
    Ok(Response::json(
        json!({ "id": user.id, "email": user.email }),
    ))
}

// ---- projects ----

#[derive(Deserialize)]
struct NewProject {
    name: String,
}

async fn create_project(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let user = current_user(&req)?;
    let body: NewProject = req.json()?;
    let name = body.name.trim().to_string();
    let mut v = Validator::new();
    v.len_between("name", &name, 1, 80);
    v.finish()?;

    let mut data = st.lock();
    let id = data.id();
    data.projects.push(Project {
        id,
        owner: user,
        name: name.clone(),
    });
    Ok(created(
        json!({ "id": id, "name": name }),
        format!("/api/projects/{id}"),
    ))
}

async fn list_projects(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let user = current_user(&req)?;
    let data = st.lock();
    let items: Vec<Value> = data
        .projects
        .iter()
        .filter(|p| p.owner == user)
        .map(|p| {
            let open = data
                .tasks
                .values()
                .filter(|t| t.project_id == p.id && t.status != "done")
                .count();
            json!({ "id": p.id, "name": p.name, "open_tasks": open })
        })
        .collect();
    Ok(Response::json(json!({ "items": items })))
}

async fn get_project(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let id: u64 = req.params.parse("id")?;
    let data = st.lock();
    let p = data.project_of(current_user(&req)?, id)?;
    Ok(Response::json(json!({ "id": p.id, "name": p.name })))
}

async fn delete_project(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let id: u64 = req.params.parse("id")?;
    let mut data = st.lock();
    data.project_of(current_user(&req)?, id)?;
    data.projects.retain(|p| p.id != id);
    data.tasks.retain(|_, t| t.project_id != id);
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

// ---- tasks ----

#[derive(Deserialize)]
struct NewTask {
    title: String,
    description: Option<String>,
    priority: Option<u8>,
    due: Option<String>,
}

fn check_task_fields(
    v: &mut Validator,
    title: Option<&str>,
    description: Option<&str>,
    priority: Option<i64>,
    due: Option<&str>,
) {
    if let Some(t) = title {
        v.len_between("title", t.trim(), 1, 120);
    }
    if let Some(d) = description {
        v.len_between("description", d, 0, 2000);
    }
    if let Some(p) = priority {
        v.range("priority", p, 0, 3);
    }
    if let Some(d) = due {
        v.check(valid_date(d), "due", "must be a date like 2026-12-31");
    }
}

async fn create_task(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let project_id: u64 = req.params.parse("id")?;
    let body: NewTask = req.json()?;
    let mut v = Validator::new();
    check_task_fields(
        &mut v,
        Some(&body.title),
        body.description.as_deref(),
        body.priority.map(i64::from),
        body.due.as_deref(),
    );
    v.finish()?;

    let mut data = st.lock();
    data.project_of(current_user(&req)?, project_id)?;
    let id = data.id();
    let task = Task {
        id,
        project_id,
        title: body.title.trim().to_string(),
        description: body.description,
        status: "open",
        priority: body.priority.unwrap_or(1),
        due: body.due,
        version: 1,
        created_seq: id,
    };
    let res = created(task.view(), format!("/api/tasks/{id}")).with_header("etag", task.etag());
    data.tasks.insert(id, task);
    Ok(res)
}

#[derive(Deserialize)]
struct TaskQuery {
    status: Option<String>,
    priority: Option<u8>,
    q: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    page: Option<usize>,
    per_page: Option<usize>,
}

async fn list_tasks(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let project_id: u64 = req.params.parse("id")?;
    let q: TaskQuery = req.query.parse()?;

    let mut v = Validator::new();
    if let Some(s) = &q.status {
        v.check(
            STATUSES.contains(&s.as_str()),
            "status",
            "must be open, in_progress or done",
        );
    }
    if let Some(s) = &q.sort {
        v.check(
            ["created", "due", "priority"].contains(&s.as_str()),
            "sort",
            "must be created, due or priority",
        );
    }
    if let Some(o) = &q.order {
        v.check(
            ["asc", "desc"].contains(&o.as_str()),
            "order",
            "must be asc or desc",
        );
    }
    v.finish()?;

    let data = st.lock();
    data.project_of(current_user(&req)?, project_id)?;
    let needle = q.q.as_ref().map(|s| s.to_lowercase());
    let mut tasks: Vec<&Task> = data
        .tasks
        .values()
        .filter(|t| t.project_id == project_id)
        .filter(|t| q.status.as_deref().is_none_or(|s| t.status == s))
        .filter(|t| q.priority.is_none_or(|p| t.priority == p))
        .filter(|t| {
            needle.as_ref().is_none_or(|n| {
                t.title.to_lowercase().contains(n)
                    || t.description
                        .as_ref()
                        .is_some_and(|d| d.to_lowercase().contains(n))
            })
        })
        .collect();

    let desc = q.order.as_deref() == Some("desc");
    let sort = q.sort.as_deref().unwrap_or("created");
    tasks.sort_by(|a, b| {
        use std::cmp::Ordering;
        let directed = |o: Ordering| if desc { o.reverse() } else { o };
        let primary = match sort {
            // undated tasks stay last in both directions
            "due" => match (&a.due, &b.due) {
                (Some(x), Some(y)) => directed(x.cmp(y)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            "priority" => directed(a.priority.cmp(&b.priority)),
            _ => directed(a.created_seq.cmp(&b.created_seq)),
        };
        // ties keep creation order whichever way the list is sorted
        primary.then(a.created_seq.cmp(&b.created_seq))
    });

    let per_page = q.per_page.unwrap_or(20).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);
    let total = tasks.len();
    let items: Vec<Value> = tasks
        .into_iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .map(Task::view)
        .collect();
    Ok(Response::json(json!({
        "items": items, "page": page, "per_page": per_page, "total": total
    })))
}

async fn get_task(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let id: u64 = req.params.parse("id")?;
    let data = st.lock();
    let task = data.task_of(current_user(&req)?, id)?;
    Ok(Response::json(task.view()).with_header("etag", task.etag()))
}

/// Partial update guarded by If-Match; `null` clears description and due
async fn patch_task(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let id: u64 = req.params.parse("id")?;
    let patch: Map<String, Value> = req.json()?;

    let mut data = st.lock();
    let current = data.task_of(current_user(&req)?, id)?.clone();
    require_if_match(&req, &current.etag())?;

    let mut v = Validator::new();
    let mut task = current;
    for (key, value) in &patch {
        match (key.as_str(), value) {
            ("title", Value::String(s)) => {
                check_task_fields(&mut v, Some(s), None, None, None);
                task.title = s.trim().to_string();
            }
            ("description", Value::Null) => task.description = None,
            ("description", Value::String(s)) => {
                check_task_fields(&mut v, None, Some(s), None, None);
                task.description = Some(s.clone());
            }
            ("priority", Value::Number(n)) => {
                check_task_fields(&mut v, None, None, n.as_i64(), None);
                task.priority = n
                    .as_i64()
                    .and_then(|p| u8::try_from(p).ok())
                    .unwrap_or(task.priority);
            }
            ("status", Value::String(s)) => match STATUSES.iter().find(|x| **x == s) {
                Some(status) => task.status = status,
                None => {
                    v.error("status", "must be open, in_progress or done");
                }
            },
            ("due", Value::Null) => task.due = None,
            ("due", Value::String(s)) => {
                check_task_fields(&mut v, None, None, None, Some(s));
                task.due = Some(s.clone());
            }
            ("title" | "description" | "priority" | "status" | "due", _) => {
                v.error(key, "has the wrong type");
            }
            _ => {
                v.error(key, "unknown field");
            }
        }
    }
    v.finish()?;

    task.version += 1;
    let res = Response::json(task.view()).with_header("etag", task.etag());
    data.tasks.insert(id, task);
    Ok(res)
}

async fn delete_task(req: Request) -> Result<Response> {
    let st = state(&req)?;
    let id: u64 = req.params.parse("id")?;
    let mut data = st.lock();
    let current = data.task_of(current_user(&req)?, id)?;
    // If-Match is optional here: deleting is idempotent enough
    if req.headers.contains_key(http::header::IF_MATCH) {
        require_if_match(&req, &current.etag())?;
    }
    data.tasks.remove(&id);
    Ok(Response::new(http::StatusCode::NO_CONTENT))
}

// ---- wiring ----

fn task_schema() -> Value {
    schema::object(
        [
            ("id", schema::integer()),
            ("project_id", schema::integer()),
            ("title", schema::string()),
            ("description", schema::string()),
            ("status", schema::string()),
            ("priority", schema::integer()),
            ("due", schema::string()),
            ("version", schema::integer()),
        ],
        ["id", "project_id", "title", "status", "priority", "version"],
    )
}

fn secured(summary: &str, tag: &str) -> Operation {
    Operation::new(summary)
        .tag(tag)
        .secured()
        .error_response(401, "Missing, invalid or expired token")
}

pub fn build(config: Config) -> App {
    let params = if config.fast_hashing {
        Params::new(8, 1, 1, None).expect("valid argon2 params")
    } else {
        Params::default()
    };
    let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let dummy_hash = hash_password(&hasher, "dummy password for timing").expect("hash");
    let (limit, window) = config.auth_rate_limit;

    let state = AppState {
        data: Mutex::new(Data::default()),
        secret: config.jwt_secret.clone(),
        token_ttl: config.token_ttl_secs,
        hasher,
        dummy_hash,
    };
    let auth_limit = Arc::new(RateLimiter::new(limit, window));

    App::new()
        .state(state)
        .max_body_size(Some(64 * 1024))
        .middleware(|req: Request, next: Next| async move {
            let res = match next.run(req).await {
                Ok(res) => res,
                Err(err) => Response::from_error(err),
            };
            Ok(res
                .with_header("x-content-type-options", "nosniff")
                .with_header("cache-control", "no-store"))
        })
        .get("/healthz", |_req: Request| async {
            Ok(Response::json(json!({ "status": "ok" })))
        })
        .group("/auth", move |g| {
            g.middleware(auth_limit)
                .post("/register", register)
                .doc(
                    "POST",
                    "/register",
                    Operation::new("Create an account")
                        .tag("auth")
                        .request_json(schema::object(
                            [("email", schema::string()), ("password", schema::string())],
                            ["email", "password"],
                        ))
                        .response(201, "Account created")
                        .error_response(409, "E-mail already registered")
                        .error_response(422, "Validation failed (per-field messages)")
                        .error_response(429, "Too many attempts"),
                )
                .post("/login", login)
                .doc(
                    "POST",
                    "/login",
                    Operation::new("Exchange credentials for a bearer token")
                        .tag("auth")
                        .request_json(schema::object(
                            [("email", schema::string()), ("password", schema::string())],
                            ["email", "password"],
                        ))
                        .response_json(
                            200,
                            "Token",
                            schema::object([("token", schema::string()), ("expires_in", schema::integer())], ["token"]),
                        )
                        .error_response(401, "Invalid e-mail or password")
                        .error_response(429, "Too many attempts"),
                )
        })
        .group("/api", move |g| {
            g.middleware(JwtAuth::new(config.jwt_secret.clone()))
                .middleware(ETags)
                .get("/me", me)
                .doc("GET", "/me", secured("The logged-in user", "auth"))
                .post("/projects", create_project)
                .doc(
                    "POST",
                    "/projects",
                    secured("Create a project", "projects")
                        .request_json(schema::object([("name", schema::string())], ["name"]))
                        .error_response(422, "Validation failed"),
                )
                .get("/projects", list_projects)
                .doc("GET", "/projects", secured("Your projects with open task counts", "projects"))
                .get("/projects/{id}", get_project)
                .doc(
                    "GET",
                    "/projects/{id}",
                    secured("One project", "projects").error_response(404, "No such project"),
                )
                .delete("/projects/{id}", delete_project)
                .doc(
                    "DELETE",
                    "/projects/{id}",
                    secured("Delete a project and its tasks", "projects")
                        .response(204, "Deleted")
                        .error_response(404, "No such project"),
                )
                .post("/projects/{id}/tasks", create_task)
                .doc(
                    "POST",
                    "/projects/{id}/tasks",
                    secured("Create a task", "tasks")
                        .request_json(schema::object(
                            [
                                ("title", schema::string()),
                                ("description", schema::string()),
                                ("priority", schema::integer()),
                                ("due", schema::string()),
                            ],
                            ["title"],
                        ))
                        .response_json(201, "Created, with ETag", task_schema())
                        .error_response(404, "No such project")
                        .error_response(422, "Validation failed"),
                )
                .get("/projects/{id}/tasks", list_tasks)
                .doc(
                    "GET",
                    "/projects/{id}/tasks",
                    secured("List tasks (filter, sort, paginate)", "tasks")
                        .query_param("status", schema::string(), "open | in_progress | done", false)
                        .query_param("priority", schema::integer(), "0-3", false)
                        .query_param("q", schema::string(), "text search in title and description", false)
                        .query_param("sort", schema::string(), "created | due | priority", false)
                        .query_param("order", schema::string(), "asc | desc", false)
                        .query_param("page", schema::integer(), "1-based", false)
                        .query_param("per_page", schema::integer(), "1-100, default 20", false)
                        .error_response(422, "Invalid filter values"),
                )
                .get("/tasks/{id}", get_task)
                .doc(
                    "GET",
                    "/tasks/{id}",
                    secured("One task (ETag header, supports If-None-Match)", "tasks")
                        .response_json(200, "The task", task_schema())
                        .response(304, "Not modified")
                        .error_response(404, "No such task"),
                )
                .patch("/tasks/{id}", patch_task)
                .doc(
                    "PATCH",
                    "/tasks/{id}",
                    secured("Partially update a task; needs If-Match with the current ETag", "tasks")
                        .response_json(200, "Updated task with the new ETag", task_schema())
                        .error_response(404, "No such task")
                        .error_response(412, "ETag is stale")
                        .error_response(422, "Validation failed")
                        .error_response(428, "If-Match header missing"),
                )
                .delete("/tasks/{id}", delete_task)
                .doc(
                    "DELETE",
                    "/tasks/{id}",
                    secured("Delete a task (If-Match optional)", "tasks")
                        .response(204, "Deleted")
                        .error_response(404, "No such task")
                        .error_response(412, "ETag is stale"),
                )
        })
        .openapi(
            OpenApiInfo::new("Task board", "1.0.0")
                .description("Projects and tasks per user. Log in via /auth/login and send the token as a bearer token.")
                .swagger_ui("/docs"),
        )
}
