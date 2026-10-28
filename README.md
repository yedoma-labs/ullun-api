# ullun-api

<picture>
  <source media="(max-width: 640px)" srcset="https://raw.githubusercontent.com/yedoma-labs/assets/main/resized/banner-resized-mobile.png">
  <img src="https://raw.githubusercontent.com/yedoma-labs/assets/main/resized/banner-resized.png" alt="Project Header">
</picture>

> **Express.js-inspired web framework for Rust** — simple, fast, batteries-included

[![CI](https://github.com/yedoma-labs/ullun-api/actions/workflows/ci.yml/badge.svg)](https://github.com/yedoma-labs/ullun-api/actions/workflows/ci.yml)
[![Security Audit](https://github.com/yedoma-labs/ullun-api/actions/workflows/audit.yml/badge.svg)](https://github.com/yedoma-labs/ullun-api/actions/workflows/audit.yml)
[![Crates.io](https://img.shields.io/crates/v/ullun-api.svg)](https://crates.io/crates/ullun-api)
[![Downloads](https://img.shields.io/crates/d/ullun-api.svg)](https://crates.io/crates/ullun-api)
[![Documentation](https://docs.rs/ullun-api/badge.svg)](https://docs.rs/ullun-api)
[![License](https://img.shields.io/crates/l/ullun-api.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-blue.svg)](https://www.rust-lang.org)

[![codecov](https://codecov.io/gh/yedoma-labs/ullun-api/branch/main/graph/badge.svg)](https://codecov.io/gh/yedoma-labs/ullun-api)
[![No Unsafe](https://img.shields.io/badge/unsafe-forbidden-success)](src/lib.rs)
[![Benchmarks](https://img.shields.io/badge/benchmarks-measured-green)](docs/PERFORMANCE.md)
[![Dependencies](https://deps.rs/repo/github/yedoma-labs/ullun-api/status.svg)](https://deps.rs/repo/github/yedoma-labs/ullun-api)
[![Conventional Commits](https://img.shields.io/badge/commits-conventional-fe5196?logo=conventionalcommits&logoColor=white)](https://www.conventionalcommits.org/)
[![Last commit](https://img.shields.io/github/last-commit/yedoma-labs/ullun-api)](https://github.com/yedoma-labs/ullun-api/commits/main)

[![GitHub stars](https://img.shields.io/github/stars/yedoma-labs/ullun-api?style=social)](https://github.com/yedoma-labs/ullun-api)
[![GitHub forks](https://img.shields.io/github/forks/yedoma-labs/ullun-api?style=social)](https://github.com/yedoma-labs/ullun-api/fork)
[![GitHub issues](https://img.shields.io/github/issues/yedoma-labs/ullun-api)](https://github.com/yedoma-labs/ullun-api/issues)
[![GitHub contributors](https://img.shields.io/github/contributors/yedoma-labs/ullun-api)](https://github.com/yedoma-labs/ullun-api/graphs/contributors)

> ⚠️ **Production Note:** Requires proper deployment (TLS, auth, rate limiting). See [Production Readiness](#%EF%B8%8F-production-readiness) below.

## Why ullun-api?

**The Problem:** Actix-web is fast but complex. Rocket is ergonomic but heavy. Axum ties you to Tokio internals.

**The Solution:** An Express.js-style framework for Rust — single-file APIs, minimal boilerplate, maximum performance.

```rust
use ullun::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        .get("/", |_req| async { Ok(Response::text("Hello, World!")) })
        .get_with_params("/hello/{name}", |params| async move {
            Ok(Response::json(serde_json::json!({
                "message": format!("Hello, {}!", params.get("name")?)
            })))
        })
        .run("127.0.0.1:3000")
        .await
        .unwrap();
}
```

## Features

**Core**
- 🚀 Built on hyper + tokio, measured numbers in [PERFORMANCE.md](docs/PERFORMANCE.md), reproducible against axum and actix-web with `scripts/bench.sh`
- 🎯 Express-style routing and `(req, next)` middleware, route groups with group-level middleware, `async fn(Request) -> Result<Response>` handlers
- 🔒 Typed query/form/JSON extraction, `App::fallback` for unknown paths, panics in handlers become a logged `500` (the connection survives), `Validator` with 422 responses, `Error` that maps to status codes
- 🍪 Cookies, forms, multipart uploads, [templates](#templates) (dependency-free engine or your own), static files
- 📖 OpenAPI 3.1 generated from your routes, optional Swagger UI
- 🔌 Server-sent events, streaming responses, WebSockets (`websocket` feature), pub/sub `Hub`, background `JobQueue`

**Security**
- 🛡️ `Cors` (exact origins, `Vary`, preflight, never a credentialed wildcard), `SecurityHeaders` with a CSP builder and per-request nonces
- 🔐 Sessions + CSRF, `ApiKeyAuth` / `BasicAuth` (constant-time, named principals), JWT (`jwt` feature), HTTPS with certificate reload and client certificates (`tls` feature), HTTP/2 via ALPN, `RequireHttps` / `Hsts`, `AllowedHosts` (host header poisoning)
- 🚦 Rate limiting (fixed or sliding window), `App::trusted_proxies` + `Request::client_ip()` (right-to-left `X-Forwarded-For`/`Forwarded` parsing that clients cannot spoof), constant-time comparison, CSPRNG tokens, SSRF guard (`resolve_public`, `HttpClient::public_only`)
- 🔁 `Idempotency` keys: a retried `POST` runs once and replays its response

**Reliability**
- 🧯 Slowloris protection (`header_read_timeout`), connection caps, `Timeout` (504), `ConcurrencyLimit` load shedding (503 + `Retry-After`), body limits per prefix
- 🩺 `Health` liveness/readiness probes and `App::shutdown_delay` so load balancers drain first; graceful shutdown with background tasks
- ⚡ `ResponseCache` with request coalescing and stale-while-revalidate; `HttpClient` with pooling, safe retries, redirects and response caps

**Observability and testing**
- 📈 Request ids, access log (text/JSON), Prometheus `Metrics`, W3C trace context (`TraceLayer`, `HttpClient::trace`)
- 🧪 `TestClient` runs an `App` in-process without sockets (cookie jar, JSON helpers); RFC 9457 `ProblemDetails`; `Accept` / `Accept-Language` negotiation
- 📤 Streaming uploads (`App::stream_body`), SPA-ready static files (precompressed variants, immutable assets, `index.html` fallback)

## ⚠️ Production Readiness

**ullun-api is production-ready for small-to-medium APIs** (<100k users), but requires proper deployment practices:

### ✅ Framework Provides
- Memory safety (zero `unsafe` code)
- Type-safe routing
- Graceful error handling
- CORS & security headers middleware

### ⚠️ User Must Implement
- **Authentication/Authorization** - Use middleware (see `examples/todo_api.rs`)
- **Rate Limiting** - `RateLimiter` middleware per instance; use a reverse proxy (nginx/Caddy) or a shared store for a global limit
- **TLS/HTTPS** - Terminate TLS at a reverse proxy or serve HTTPS directly with `App::tls` (never expose raw HTTP in production)
- **Input Validation** - Validate business logic (framework validates types)
- **SQL Injection Prevention** - Use parameterized queries (sqlx, diesel)
- **XSS Protection** - Sanitize HTML output (if serving HTML)

### 📋 Pre-Production Checklist
1. ✅ Implement authentication middleware
2. ✅ Terminate TLS at a reverse proxy (nginx/Caddy) or with `App::tls`
3. ✅ Add monitoring/observability (Prometheus, Sentry)
4. ✅ Follow [SECURITY.md](SECURITY.md) checklist
5. ✅ Load test your specific workload (`wrk`, `hey`, `ab`)
6. ✅ Set up error logging and alerting

**See [SECURITY.md](SECURITY.md) for comprehensive security guidelines.**

---

## Installation

```toml
[dependencies]
ullun-api = "0.4"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Optional features (everything else is always on):

| Feature | Adds | Extra dependencies |
|---------|------|--------------------|
| `jwt` | `ullun::jwt` - HS256 tokens, `JwtAuth` middleware | `hmac`, `sha2`, `base64` |
| `signing` | `ullun::signed_cookies` - tamper-proof cookies with key rotation (HMAC-SHA256) | `hmac`, `sha2`, `base64` |
| `jwt-asymmetric` | `ullun::jwt_keys` - RS256 / ES256 tokens, JWKS parsing, `JwtAuth::with_keys` | `ring` |
| `redis` | `ullun::redis` - Redis client and shared stores for sessions, rate limits and idempotency keys across instances | none |
| `openapi-schemars` | `Operation::request_type` / `response_type` / `query_type`: OpenAPI schemas derived from `schemars::JsonSchema` types | `schemars` |
| `encryption` | `ullun::private_cookies` - encrypted, tamper-proof cookies with key rotation (XChaCha20-Poly1305) | `chacha20poly1305`, `base64` |
| `compression` | `ullun::compression::Compression` - gzip middleware | `flate2` |
| `websocket` | `App::ws`, `ullun::websocket` | `tokio-tungstenite`, `futures-util` |
| `client-tls` | `ullun::client` over HTTPS (plain `http://` works without it) | `tokio-rustls`, `webpki-roots` |
| `tls` | `App::tls`, `ullun::tls` - HTTPS, certificate reload, client certificates | `tokio-rustls` (rustls + ring) |
| `full` | all of the above | |

```toml
ullun-api = { version = "0.4", features = ["jwt", "websocket"] }
```

## Quick Start

### Hello World

```rust
use ullun::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        .get("/", |_req| async {
            Ok(Response::text("Hello, World!"))
        })
        .run("127.0.0.1:3000")
        .await
        .unwrap();
}
```

### Path Parameters

```rust
App::new()
    .get_with_params("/users/{id}", |params| async move {
        let id: u64 = params.get("id")?.parse()
            .map_err(|_| Error::bad_request("Invalid user ID"))?;
        
        Ok(Response::json(serde_json::json!({
            "user_id": id
        })))
    })
    .run("127.0.0.1:3000")
    .await
    .unwrap();
```

### JSON Request Body

```rust
#[derive(Deserialize)]
struct CreateUser {
    name: String,
    email: String,
}

async fn create_user(req: Request) -> Result<Response> {
    let body: CreateUser = req.json()?;
    
    Ok(Response::json(serde_json::json!({
        "message": format!("Created user: {}", body.name)
    })))
}

App::new()
    .post("/users", create_user)
    .run("127.0.0.1:3000")
    .await
    .unwrap();
```

### Middleware

```rust
use ullun::cors::Cors;
use ullun::middleware::logger;
use ullun::security_headers::{Csp, SecurityHeaders};

App::new()
    .middleware(logger)
    .middleware(Cors::new().allow_origin("https://app.example.com").allow_credentials())
    .middleware(SecurityHeaders::new().csp(Csp::api()))
    .get("/", |_req| async { Ok(Response::text("Hello!")) })
    .run("127.0.0.1:3000")
    .await
    .unwrap();
```

### Route Groups (v0.2.0)

```rust
App::new()
    .group("/api", |group| {
        group
            .get("/users", list_users)
            .post("/users", create_user)
    })
    .group("/admin", |group| {
        group
            .get("/dashboard", admin_dashboard)
            .post("/settings", update_settings)
    })
    .run("127.0.0.1:3000")
    .await?;
```

### Cookie Handling (v0.2.0)

```rust
use ullun::cookies::{Cookie, SameSite};

// Reading cookies
async fn get_session(req: Request) -> Result<Response> {
    let cookies = req.cookies();
    if let Some(session_id) = cookies.get("session") {
        Ok(Response::text(format!("Session: {}", session_id)))
    } else {
        Err(Error::unauthorized("Not logged in"))
    }
}

// Setting cookies
async fn login(_req: Request) -> Result<Response> {
    let cookie = Cookie::new("session", "abc123")
        .path("/")
        .max_age(3600)
        .http_only()
        .secure()
        .same_site(SameSite::Lax);
    
    Ok(Response::text("Logged in").cookie(cookie))
}
```

### Static Files (v0.2.0)

```rust
App::new()
    .serve_static("/", "public")        // Serve from 'public' directory
    .serve_static("/assets", "dist")    // Serve from 'dist' at /assets
    .run("127.0.0.1:3000")
    .await?;
```

A single-page app built by a bundler:

```rust
use ullun::static_files::StaticOptions;

App::new()
    .serve_static_with("/", "dist", StaticOptions::new()
        .precompressed()              // app.js.br / app.js.gz next to app.js, chosen by Accept-Encoding
        .immutable_dir("assets")      // cached for a year, never revalidated
        .spa_fallback("index.html"))  // browser navigations to /orders/17 get index.html
```

### Body Size Limits (v0.2.0)

```rust
App::new()
    .max_body_size(Some(5 * 1024 * 1024))  // 5 MB limit
    // or
    .max_body_size(None)                    // Unlimited (not recommended)
    .run("127.0.0.1:3000")
    .await?;
```

### Shared State

```rust
struct Db { users: RwLock<Vec<User>> }

App::new()
    .state(Db::default())                          // one value per type, shared by all requests
    .get("/users", |req: Request| async move {
        let db = req.state::<Db>()?;               // 500 if it was never registered
        Ok(Response::json(&*db.users.read().await))
    })
```

### Security Helpers

```rust
use ullun::security::{constant_time_eq, random_token, random_alphanumeric};

let session_id = random_token(16)?;                // 128 bits from the OS CSPRNG, 32 hex chars
let short_code = random_alphanumeric(7)?;          // uniform [A-Za-z0-9]
let ok = constant_time_eq(given.as_bytes(), expected.as_bytes());   // for API keys, delete tokens, ...
```

### HTTPS (feature `tls`)

```rust
let tls = TlsConfig::from_pem_files("fullchain.pem", "privkey.pem")?
    .client_auth(&std::fs::read("clients-ca.pem")?, false)?;   // optional mutual TLS

App::new()
    .middleware(Hsts::new())                                    // Strict-Transport-Security on secure requests
    .get("/admin", |req: Request| async move {
        let cert = req.client_certificate().ok_or_else(|| Error::unauthorized("client certificate required"))?;
        // verified against your CA already; decide whether *this* certificate may do this (fingerprint allow-list)
        Ok(Response::text("ok"))
    })
    .tls(tls)
    .tls_reload_every(Duration::from_secs(3600))                // renewed certificates are picked up, no restart
    .run("0.0.0.0:8443")
    .await?;

// port 80: only redirect
App::new().middleware(RequireHttps::new().https_port(8443)).run("0.0.0.0:8080").await?;
```

TLS 1.2/1.3 via rustls (ring), **HTTP/2 negotiated through ALPN** (multiplexed requests, up to 128 concurrent streams per connection, dead peers dropped by keep-alive pings; HTTP/1.1 clients keep working on the same port), handshakes with a timeout, a broken renewal keeps the old certificate in service, WebSockets work as `wss`. `req.is_secure()` tells handlers how the request arrived; behind a TLS-terminating proxy use `trust_forwarded_proto()` on `RequireHttps` / `Hsts` (only if the proxy overwrites `X-Forwarded-Proto`).

### Background jobs

```rust
let queue: JobQueue<SendMail> = JobQueue::new(QueueConfig::new().max_attempts(5).persist_to("queue.json"));
let worker = queue.clone();

App::new()
    .state(queue)
    .post("/signup", |req: Request| async move {
        req.state::<JobQueue<SendMail>>()?.enqueue(SendMail { to: "ann@example.com".into() });
        Ok(Response::text("welcome"))                          // answered at once, mail goes out later
    })
    .background(move |ctx: Context| async move {
        worker.run(ctx.shutdown(), 4, |job| async move {
            deliver(&job.payload).await.map_err(|e| JobError::retry(e.to_string()))   // or retry_after / fatal
        }).await;
    })
```

Exponential backoff with jitter, a dead-letter list (`dead_jobs`, `retry_dead`, `delete_dead`), delayed jobs and de-duplication keys (`enqueue_with`), panics counted as failed attempts, and a clean stop that lets running jobs finish. Delivery is **at-least-once** (a job interrupted by a crash runs again), so handlers must be safe to repeat.

### Safe outgoing requests

```rust
let addr = resolve_public(host, port, false).await?;   // every DNS answer must be a public address
let stream = TcpStream::connect(addr).await?;          // connect to the vetted address, not to the name again
```

`ullun::security::resolve_public` / `is_public_ip` keep user-supplied URLs from reaching loopback, private networks, cloud metadata endpoints or addresses that merely embed them (IPv4-mapped, NAT64, 6to4), and cannot be redirected by DNS rebinding.

### Rooms (publish/subscribe)

```rust
let hub: Hub<ChatEvent> = Hub::new(64);              // 64 buffered messages per room
let mut sub = hub.subscribe("general");              // leaves the room when dropped
hub.publish("general", event);                       // fan-out to every member
match sub.recv().await {
    Some(Delivery::Message(e)) => { /* forward to this client */ }
    Some(Delivery::Missed(n))  => { /* this client was too slow and lost n messages */ }
    None => {}
}
```

Slow members never hold the others back, and a room disappears with its last member. Combined with `App::ws` this is a chat server in a few dozen lines (see `examples/chat`). WebSockets ping idle peers every 30 s and drop silent ones after 90 s (`WsOptions::ping_interval` / `idle_timeout`); `RouteGroup::ws_with` lets group middleware, such as a ticket check, run before the upgrade.

### Observability

```rust
let metrics = Metrics::new();
App::new()
    .middleware(RequestIdLayer::new())            // x-request-id: kept if sane, else generated
    .middleware(AccessLog::new().json())          // one line per request; query strings left out
    .middleware(metrics.clone())                  // counters + latency histogram + in-flight gauge
    .get("/metrics", metrics.handler())           // Prometheus text format (keep it internal)
```

Logs and metrics identify a request by its route pattern (`/users/{id}`, from `Request::matched_path`), so label cardinality follows your routes, not your ids or scanner traffic. Handlers read the id with `req.request_id()`.

### Sessions and CSRF

```rust
App::new()
    .middleware(SessionLayer::new())             // cookie holds only an unguessable id
    .middleware(Csrf::new())                     // POST/PUT/PATCH/DELETE need the session's token
    .post("/login", |req: Request| async move {
        let session = req.session()?;
        session.renew();                         // fresh id on login: no session fixation
        session.set("user_id", 42)?;
        session.flash("Welcome back")?;          // shown once, e.g. after the redirect
        Ok(Response::redirect("/"))
    })
    .get("/form", |req: Request| async move {
        let token = req.session()?.csrf_token()?;   // <input type="hidden" name="csrf_token" value="{token}">
        Ok(Response::html(render_form(&token)))
    })
```

`SessionLayer::insecure_cookies()` for plain-HTTP development, `SessionLayer::with_store(..)` for Redis or a database, `Csrf::exempt(..)` for bearer-token API calls. Anonymous visitors never receive a session cookie unless you write to the session.

### File Uploads and Downloads

```rust
async fn upload(req: Request) -> Result<Response> {
    let form = req.multipart()?;                              // 415 / 400 / 413 on bad input
    let file = form.file("avatar").ok_or_else(|| Error::bad_request("avatar is required"))?;
    let shown_name = file.safe_filename().unwrap_or_else(|| "file".into());   // never a path!
    // store the bytes under a name YOU generate (random_alphanumeric), not the client's
    Ok(Response::redirect("/profile"))
}

async fn download(req: Request) -> Result<Response> {
    // streams large files; Range / If-None-Match / If-Modified-Since / HEAD all work
    serve_file(&req, path_you_chose, &FileOptions::new().attachment("report.pdf")).await
}

App::new()
    .max_body_size(Some(64 * 1024))                           // small default ...
    .body_limit("/upload", Some(25 << 20))                    // ... bigger where uploads happen
    .serve_static("/assets", "public")                        // dotfiles hidden, traversal-proof
```

Uploaded HTML, SVG and scripts must never be served inline from your own origin: send them as `attachment` (see `examples/drive`).

### HEAD, OPTIONS and 405

`HEAD` is answered by the `GET` handler (headers only). A known path with the wrong method gets `405` with an `Allow` header, and `OPTIONS` on a known path answers `204` with `Allow`. CORS preflights are still handled by the `Cors` middleware.

### Streaming and Server-Sent Events

```rust
use ullun::sse::{sse, Event};

async fn ticker(req: Request) -> Result<Response> {
    let (tx, res) = sse();                       // text/event-stream, no caching, no proxy buffering
    let shutdown = req.shutdown_signal();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.wait() => return,   // end with the server
                _ = tx.closed() => return,       // browser went away
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if tx.send(Event::data("tick").event("tick")).await.is_err() { return; }
                }
            }
        }
    });
    Ok(res)
}
```

`Response::stream()` gives the same for arbitrary bodies (downloads, exports): chunks are sent as they are produced, with back-pressure, and `send` fails once the client disconnects. `ETags` and `Compression` leave streams alone.

### Background Tasks and Graceful Shutdown

```rust
App::new()
    .state(Jobs::default())
    .background(|ctx: Context| async move {
        while ctx.sleep(Duration::from_secs(60)).await {     // false once shutdown begins
            ctx.state::<Jobs>().unwrap().purge_expired();
        }
    })
    .shutdown_grace(Duration::from_secs(15))                  // default 10 s
    .run("0.0.0.0:3000")                                      // stops on CTRL+C and SIGTERM
```

On shutdown the server stops accepting, closes idle keep-alive connections, lets running requests finish, signals `Request::shutdown_signal()` / `Context::shutdown()`, and waits for background tasks, each for at most the grace period. For embedding and tests use `serve_with_shutdown(listener, signal)`.

### Validation

```rust
use ullun::validate::Validator;

let mut v = Validator::new();
v.len_between("title", &body.title, 1, 120);
v.email("email", &body.email);
v.range("priority", body.priority, 0, 3);
v.finish()?;      // 422 {"error":"Validation failed","fields":{"title":["..."],"priority":["..."]}}
```

All problems are reported at once. Typed path parameters: `let id: u64 = req.params.parse("id")?;` (400 naming the parameter).

### ETags and optimistic concurrency

```rust
use ullun::conditional::{require_if_match, ETags};

App::new()
    .middleware(ETags)                                   // ETag on GET responses, 304 for If-None-Match
    .patch("/tasks/{id}", |req: Request| async move {
        require_if_match(&req, &current_etag)?;          // 428 without If-Match, 412 when stale
        // ... apply the update, bump the version, return the new ETag
    })
```

### Group Middleware

```rust
App::new()
    .middleware(logger)                       // every request, including 404s
    .group("/admin", |g| {
        g.middleware(require_admin)           // only the routes of this group
            .get("/dashboard", dashboard)
    })
```

### Forms

```rust
#[derive(Deserialize)]
struct Signup { name: String, age: u8, newsletter: Option<bool> }

async fn signup(req: Request) -> Result<Response> {
    let form: Signup = req.form()?;           // 415 for other content types, 400 for bad values
    // req.form_data()? gives decoded key/value access instead
    Ok(Response::redirect("/thanks"))         // 303 See Other
}
```

`req.query.parse::<T>()` converts numbers, booleans and options the same way.

### Templates

```rust
let mut templates = Templates::new();         // or Templates::from_dir("templates")?
templates.add("hello.html", "Hello {{name}}!{{#each tags}} #{{this}}{{/each}}")?;

async fn hello(templates: Arc<Templates>) -> Result<Response> {
    Ok(Response::html(templates.render("hello.html", &json!({ "name": "<Ann>", "tags": ["a", "b"] }))?))
}
```

Variables are HTML-escaped (`{{{raw}}}` is not), with `{{#if}}` / `{{else}}` / `{{#unless}}`, `{{#each}}`, `{{> partial}}` and `{{! comments }}`. Using minijinja, tera or handlebars instead? Implement `TemplateEngine` (one method) and keep the same call sites.

### Rate Limiting

```rust
use ullun::rate_limit::RateLimiter;

App::new()
    .middleware(RateLimiter::new(100, Duration::from_secs(60)))   // per client IP
    // .middleware(RateLimiter::new(5, Duration::from_secs(10)).key_by(|req| api_key(req)))
```

Answers `429` with `retry-after` and `x-ratelimit-*` headers. Behind a reverse proxy use `.trust_forwarded_for()`; never without one, because clients could pick their own bucket.

### OpenAPI

```rust
use ullun::openapi::{schema, OpenApiInfo, Operation};

App::new()
    .get_with_params("/users/{id}", get_user)
    .doc("GET", "/users/{id}",
        Operation::new("Fetch a user").tag("users")
            .path_param("id", schema::integer(), "User id")
            .response_json(200, "The user", schema::object([("id", schema::integer())], ["id"]))
            .error_response(404, "No such user"))
    .openapi(OpenApiInfo::new("Users API", "1.0.0").swagger_ui("/docs"))   // /openapi.json + /docs
```

All routes are listed automatically, `.doc` adds the details. Swagger UI is opt-in because it loads a script from unpkg (pinned, with integrity hashes).

### JWT (feature `jwt`)

```rust
let token = jwt::encode(&json!({ "sub": "ann", "exp": jwt::expires_in(900) }), secret)?;

App::new().group("/api", |g| {
    g.middleware(JwtAuth::new(secret))        // 401 without a valid bearer token
        .get("/me", |req: Request| async move {
            let claims = req.extensions.get::<Claims>().unwrap();
            Ok(Response::json(&claims.0))
        })
})
```

HS256 only (`alg: none` and friends are rejected), constant-time verification, `exp`/`nbf`/issuer/audience checks, secrets of at least 32 bytes.

### Compression (feature `compression`)

```rust
App::new().middleware(Compression::new())     // gzip text/JSON/XML/JS bodies >= 1 KiB when accepted
```

### WebSockets (feature `websocket`)

```rust
App::new().ws("/chat/{room}", |req: Request, mut socket: WebSocket| async move {
    while let Some(Ok(msg)) = socket.recv().await {
        if let Message::Text(t) = msg {
            let _ = socket.send_text(t).await;
        }
    }
})
```

Cross-site `Origin`s are rejected unless listed in `WsOptions::allowed_origins` (`App::ws_with`), messages are capped at 1 MiB.

## API Reference

### App

```rust
let app = App::new()
    .get(path, handler)           // GET route
    .post(path, handler)          // POST route
    .put(path, handler)           // PUT route
    .delete(path, handler)        // DELETE route
    .get_with_params(path, handler) // GET with path params extractor
    .middleware(middleware)       // Add middleware
    .run(addr);                   // Start server (graceful shutdown on CTRL+C)
```

Need the bound port, or no signal handler (tests, benchmarks, embedding)? Bind yourself and use `serve`:

```rust
let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
println!("port {}", listener.local_addr()?.port());
app.serve(listener).await?;
```

Middleware runs for every request, including unmatched routes (404) and CORS preflight (`OPTIONS`).

### Handlers

Handlers can be:
- Async functions taking `Request` → `Result<Response>`
- Closures with path params: `Params` → `Result<Response>`

```rust
// Full request access
async fn handler(req: Request) -> Result<Response> {
    let body = req.json::<MyType>()?;
    Ok(Response::json(body))
}

// Path params only
.get_with_params("/users/{id}", |params| async move {
    let id = params.get("id")?;
    // ...
})
```

### Error Handling

Errors automatically map to HTTP status codes:

```rust
return Err(Error::bad_request("Invalid input"));      // 400
return Err(Error::unauthorized("Login required"));    // 401
return Err(Error::forbidden("Access denied"));        // 403
return Err(Error::not_found("User not found"));       // 404
return Err(Error::internal("Database error"));        // 500
return Err(Error::custom(418, "I'm a teapot"));       // Custom
```

### Middleware

Create custom middleware:

```rust
async fn my_middleware(req: Request, next: Next) -> Result<Response> {
    // Before handler
    println!("Before: {}", req.uri);
    
    let response = next.run(req).await?;
    
    // After handler
    println!("After: {}", response.status);
    
    Ok(response)
}

App::new()
    .middleware(my_middleware)
    // ...
```

## Examples

See the [`examples/`](examples/) directory. Basics:

- [`hello_world.rs`](examples/hello_world.rs) - Minimal example
- [`full_api.rs`](examples/full_api.rs) - CRUD API with middleware
- [`todo_api.rs`](examples/todo_api.rs) - TODO API with token authentication
- [`complete_api.rs`](examples/complete_api.rs) - In-memory store, sessions, groups
- [`features_0_2_0.rs`](examples/features_0_2_0.rs) - Route groups, cookies, static files

### Real-world applications

Complete services that run as they are and are covered by integration tests (`tests/example_*.rs` start the very same app module over real HTTP). Each lives in `examples/<name>/` with a `main.rs` (configuration from the environment) and an `app.rs` (the testable application).

| App | Run | What it demonstrates |
|-----|-----|----------------------|
| [`production`](examples/production) | `cargo run --example production` | Service skeleton with the middleware in a sensible order: trace + request ids + JSON access log + metrics, security headers, CORS, problem+json, timeout / concurrency / sliding rate limit, API-key admin area, cached catalog, idempotent orders, trusted proxies, probes with shutdown drain |
| [`orders`](examples/orders) | `cargo run --example orders` | Checkout API for a browser shop: CORS with credentials, security headers, required idempotency keys (retries place one order), no overselling, timeout and load shedding |
| [`shortener`](examples/shortener) | `cargo run --example shortener` | URL shortener: API-key management API, public redirects, click counting, expiry, per-key rate limit, JSON file persistence, OpenAPI |
| [`taskboard`](examples/taskboard) (`jwt`) | `DEV=1 cargo run --example taskboard --features jwt` | REST API: registration/login (argon2 + JWT), per-user projects and tasks, 422 field errors, filter/sort/pagination, ETag + If-Match, login throttling, OpenAPI + Swagger UI |
| [`statuspage`](examples/statuspage) | `DEV=1 cargo run --example statuspage` | Uptime monitor: background scheduler, HTTP/TCP checks with SSRF protection, public status page with live Server-Sent Events, admin API, graceful SIGTERM shutdown |
| [`secure_api`](examples/secure_api) (`tls`) | `DEV=1 cargo run --example secure_api --features tls` | HTTPS service: self-renewing certificates, HTTP->HTTPS redirect listener, HSTS, mutual-TLS admin routes with a fingerprint allow-list |
| [`webhooks`](examples/webhooks) | `DEV=1 cargo run --example webhooks` | Outgoing webhooks: signed deliveries, retries with backoff and Retry-After, dead letters, endpoint auto-disable, idempotent publishing, SSRF-safe targets, persistence |
| [`chat`](examples/chat) (`websocket`, `jwt`) | `DEV=1 cargo run --example chat --features websocket,jwt` | Chat rooms over WebSockets: one-time tickets instead of tokens in URLs, history, presence, rate limits, connection limits, graceful close |
| [`gateway`](examples/gateway) | `DEV=1 cargo run --example gateway` | Reverse proxy: prefix routing, round-robin, failover for idempotent requests, passive health checks, 502/504, streaming relay, forwarded headers, traversal-proof paths, access log + metrics |
| [`blog`](examples/blog) | `DEV=1 cargo run --example blog` | Blog/CMS: Markdown posts (escaped HTML, safe links), admin login with argon2, sessions renewed on login, CSRF on every form, throttled login, strict CSP, RSS and sitemap, drafts, ETags |
| [`drive`](examples/drive) | `DEV=1 cargo run --example drive` | File drive: multipart uploads, per-user quotas, resumable downloads (Range), expiring share links with download limits, sandboxed risky types, per-route body limits |
| [`pastebin`](examples/pastebin) | `cargo run --example pastebin` | Paste service: HTML form + JSON API, escaped views with CSP, expiry, burn-after-read, delete tokens, storage cap, shared rate-limit budget |

Scenarios (each is a self-contained deployment shape):

| Scenario | Example | Shows |
|----------|---------|-------|
| Containerised microservice | [`microservice.rs`](examples/microservice.rs) | `/healthz` + `/readyz` probes, request ids, Prometheus metrics, `PORT` config, graceful shutdown |
| Public API for third parties | [`public_api.rs`](examples/public_api.rs) | API keys, per-key `RateLimiter`, CORS preflight, versioned group, typed query pagination, OpenAPI |
| SPA + backend in one binary | [`spa_backend.rs`](examples/spa_backend.rs) | Static frontend at `/`, JSON API under `/api`, security headers |
| Webhook receiver | [`webhook_receiver.rs`](examples/webhook_receiver.rs) | Constant-time token check, body limit, idempotency, `202` + background worker |
| Server-rendered HTML | [`html_forms.rs`](examples/html_forms.rs) | Form posts, cookie sessions, flash messages, Post/Redirect/Get, HTML escaping |
| Token-secured API | [`jwt_auth.rs`](examples/jwt_auth.rs) (`jwt`) | Login, bearer-protected group, roles, OpenAPI with security scheme and Swagger UI |
| Real-time app | [`realtime_chat.rs`](examples/realtime_chat.rs) (`websocket`) | Broadcast chat, origin check, browser client |
| Templated site | [`templates_site.rs`](examples/templates_site.rs) (`compression`) | Template engine, validated forms, gzip, JSON twin of the page |

Run examples:

```bash
cargo run --example hello_world
cargo run --example microservice
PORT=8080 cargo run --example public_api   # scenario examples read PORT (default 3000)
cargo run --example jwt_auth --features jwt   # feature-gated examples need their feature
```

Test the API:

```bash
# GET /
curl http://localhost:3000/

# GET with path params
curl http://localhost:3000/hello/world

# POST with JSON
curl -X POST http://localhost:3000/users \
  -H "Content-Type: application/json" \
  -d '{"name":"Alice","email":"alice@example.com"}'
```

## Comparison

| Framework | Learning Curve | Middleware | Style |
|-----------|----------------|------------|-------|
| Actix-web | High | Complex | Actor/extractor based |
| Rocket | Medium | Fairings | Macro/attribute based |
| Axum | Medium | Tower layers | Extractor based |
| **ullun-api** | **Low** | **Express-style** `(req, next)` | Closures and `async fn(Request)` |

This table is qualitative. We do not publish cross-framework speed claims; see [PERFORMANCE.md](docs/PERFORMANCE.md) for what is measured.

## Testing & Benchmarks

```bash
# Run tests
cargo test

# Run benchmarks (micro + end-to-end HTTP)
cargo bench

# Compare against axum and actix-web on your machine (portable, Markdown/JSON report)
scripts/bench.sh --quick

# Check coverage
cargo tarpaulin --out Html
```

See [TESTING.md](docs/TESTING.md) and [PERFORMANCE.md](docs/PERFORMANCE.md) for details.

## Security

✅ **No `unsafe` code**  
✅ **Memory safe** (Rust guarantees)  
✅ **No unwrap() in production paths**  
✅ **Header validation** (prevents malformed headers)  
✅ **Graceful error handling** (no panics)  

⚠️ **User Responsibilities:**
- Authentication/authorization implementation
- Rate limiting (reverse proxy or middleware)
- Input validation (business logic)
- SQL injection prevention (parameterized queries)
- XSS protection (if serving HTML)

See [SECURITY.md](SECURITY.md) for comprehensive guidelines and best practices.

## Roadmap

### Done
- [x] Routing, groups, middleware, cookies, forms, multipart, static files, templates, OpenAPI
- [x] Sessions, CSRF, JWT, rate limiting (fixed and sliding window), CORS, security headers, idempotency keys
- [x] TLS with reload and client certificates, HTTP/2 over ALPN, WebSockets, SSE, streaming bodies
- [x] Health probes, graceful shutdown with drain delay, connection and concurrency limits, response cache
- [x] Request ids, access log, metrics, W3C trace context, outbound HTTP client, in-process `TestClient`
- [x] OpenAPI schemas derived from types (`openapi-schemars` feature, via `schemars`)
- [x] Shared Redis stores for sessions, rate limits and idempotency keys (`redis` feature, zero dependencies)
- [x] Cleartext HTTP/2 with prior knowledge (`App::h2c`) and HTTP/2 in the outbound client (ALPN on https, `http2_prior_knowledge` on http)
- [x] RS256/ES256 JWTs and JWKS (`jwt-asymmetric` feature)
- [x] Encrypted ("private") cookies (`encryption` feature)
- [x] Portable benchmark against axum and actix-web (`scripts/bench.sh`)

### Custom stores (SQL, ...)

Sessions, rate limits and idempotency keys keep their state behind three small traits: `SessionStore`, `RateLimitStore` and `IdempotencyStore`. The Redis stores (feature `redis`) are one implementation; a SQL database is another. ullun-api ships none so that it does not pick a database library for you. A session store on top of your own pool looks like this:

```rust,ignore
use async_trait::async_trait;
use ullun::session::{SessionRecord, SessionStore};

struct PgSessions(sqlx::PgPool);

#[async_trait]
impl SessionStore for PgSessions {
    async fn load(&self, id: &str) -> Option<SessionRecord> {
        // SELECT data, created_at, last_seen FROM sessions WHERE id = $1 AND expires_at > now()
        todo!()
    }
    async fn save(&self, id: &str, record: &SessionRecord) {
        // INSERT ... ON CONFLICT (id) DO UPDATE, with an expires_at column
        todo!()
    }
    async fn delete(&self, id: &str) {
        // DELETE FROM sessions WHERE id = $1
        todo!()
    }
}

// App::new().middleware(SessionLayer::with_store(PgSessions(pool)))
```

Remember to expire old rows, and with a store shared between instances set `Idempotency::shared_seed`.

## CI/CD

GitHub Actions (`.github/workflows/ci.yml`):
- ✅ **Lint:** `rustfmt`, `clippy -D warnings`, rustdoc, `cargo audit` on every PR and on `main`
- ✅ **Test:** Linux, macOS, Windows
- ✅ **Build:** release build of all examples, benchmarks compile
- ✅ **Commits:** PR title and commits must be [Conventional Commits](https://www.conventionalcommits.org/)
- ✅ **Release:** a separate workflow (`release.yml`) runs once a day and on demand, not on every merge. Once CI is green for `main` it lets [release-plz](https://release-plz.dev) derive the next version from the commits, maintain a release PR with the changelog, and on merge tag, create the GitHub release and publish to crates.io

See [CONTRIBUTING.md](CONTRIBUTING.md#releases) for the details and required secrets.

## Contributing

Contributions welcome! Commits follow [Conventional Commits](https://www.conventionalcommits.org/); see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Documentation

- [Architecture Guide](docs/ARCHITECTURE.md) - Technical deep-dive
- [Deployment Guide](docs/DEPLOYMENT.md) - Production deployment
- [Testing Guide](docs/TESTING.md) - Testing strategies
- [Performance Guide](docs/PERFORMANCE.md) - Optimization tips
- [Security Policy](SECURITY.md) - Security best practices

## Acknowledgments

- Built on [hyper](https://hyper.rs/) and [tokio](https://tokio.rs/)
- Routing by [matchit](https://github.com/ibraheemdev/matchit)
- Inspired by [Express.js](https://expressjs.com/)

---

**Made with ❤️ by [yedoma-labs](https://github.com/yedoma-labs)**
