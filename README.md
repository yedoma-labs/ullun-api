# ullun-api

<picture>
  <source media="(max-width: 640px)" srcset="https://raw.githubusercontent.com/yedoma-labs/assets/main/resized/banner-resized-mobile.png">
  <img src="https://raw.githubusercontent.com/yedoma-labs/assets/main/resized/banner-resized.png" alt="Project Header">
</picture>

> **Express.js-inspired web framework for Rust** — simple, fast, batteries-included

[![CI](https://github.com/yedoma-labs/ullun-api/actions/workflows/ci.yml/badge.svg)](https://github.com/yedoma-labs/ullun-api/actions/workflows/ci.yml)
[![Security Audit](https://github.com/yedoma-labs/ullun-api/workflows/Security%20Audit/badge.svg)](https://github.com/yedoma-labs/ullun-api/actions)
[![Crates.io](https://img.shields.io/crates/v/ullun-api.svg)](https://crates.io/crates/ullun-api)
[![Downloads](https://img.shields.io/crates/d/ullun-api.svg)](https://crates.io/crates/ullun-api)
[![Documentation](https://docs.rs/ullun-api/badge.svg)](https://docs.rs/ullun-api)
[![License](https://img.shields.io/crates/l/ullun-api.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-blue.svg)](https://www.rust-lang.org)

[![codecov](https://codecov.io/gh/yedoma-labs/ullun-api/branch/main/graph/badge.svg)](https://codecov.io/gh/yedoma-labs/ullun-api)
[![No Unsafe](https://img.shields.io/badge/unsafe-0%25-success)](#security)
[![Benchmarks](https://img.shields.io/badge/benchmarks-measured-green)](docs/PERFORMANCE.md)

[![GitHub stars](https://img.shields.io/github/stars/yedoma-labs/ullun-api?style=social)](https://github.com/yedoma-labs/ullun-api)
[![GitHub forks](https://img.shields.io/github/forks/yedoma-labs/ullun-api?style=social)](https://github.com/yedoma-labs/ullun-api/fork)
[![GitHub issues](https://img.shields.io/github/issues/yedoma-labs/ullun-api)](https://github.com/yedoma-labs/ullun-api/issues)
[![GitHub contributors](https://img.shields.io/github/contributors/yedoma-labs/ullun-api)](https://github.com/yedoma-labs/ullun-api/graphs/contributors)

> ⚠️ **Production Note:** Requires proper deployment (TLS, auth, rate limiting). See [Production Readiness](#%EF%B8%8F-production-readiness) below.

*ullun* (уллун; Yakutian/Sakha) = "large/great" — powerful yet simple

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

- **🚀 Fast:** Built on hyper + tokio, measured numbers in [PERFORMANCE.md](docs/PERFORMANCE.md)
- **🎯 Simple:** Express.js-style routing, zero boilerplate
- **🔒 Type-safe:** Path parameters validated at compile-time
- **🔗 Middleware:** Chain middleware like Express.js
- **📦 Batteries-included:** JSON serialization, error handling, CORS built-in
- **🎨 Ergonomic:** Result<T, E> maps to HTTP status codes automatically
- **🍪 Cookie Support:** Parse and set HTTP cookies with ease
- **📁 Static Files:** Serve static files from directories
- **🎯 Route Groups:** Organize routes with common prefixes

## ⚠️ Production Readiness

**ullun-api is production-ready for small-to-medium APIs** (<100k users), but requires proper deployment practices:

### ✅ Framework Provides
- Memory safety (zero `unsafe` code)
- Type-safe routing
- Graceful error handling
- CORS & security headers middleware

### ⚠️ User Must Implement
- **Authentication/Authorization** - Use middleware (see `examples/todo_api.rs`)
- **Rate Limiting** - Use reverse proxy (nginx/Caddy) or custom middleware
- **TLS/HTTPS** - Use reverse proxy (never expose raw HTTP in production)
- **Input Validation** - Validate business logic (framework validates types)
- **SQL Injection Prevention** - Use parameterized queries (sqlx, diesel)
- **XSS Protection** - Sanitize HTML output (if serving HTML)

### 📋 Pre-Production Checklist
1. ✅ Implement authentication middleware
2. ✅ Use reverse proxy (nginx/Caddy) for TLS termination
3. ✅ Add monitoring/observability (Prometheus, Sentry)
4. ✅ Follow [SECURITY.md](SECURITY.md) checklist
5. ✅ Load test your specific workload (`wrk`, `hey`, `ab`)
6. ✅ Set up error logging and alerting

**See [SECURITY.md](SECURITY.md) for comprehensive security guidelines.**

---

## Installation

```toml
[dependencies]
ullun-api = "0.2"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
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
use ullun::middleware::{logger, cors};

App::new()
    .middleware(logger)
    .middleware(cors(vec!["*".to_string()]))
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

### Body Size Limits (v0.2.0)

```rust
App::new()
    .max_body_size(Some(5 * 1024 * 1024))  // 5 MB limit
    // or
    .max_body_size(None)                    // Unlimited (not recommended)
    .run("127.0.0.1:3000")
    .await?;
```

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

Scenarios (each is a self-contained deployment shape):

| Scenario | Example | Shows |
|----------|---------|-------|
| Containerised microservice | [`microservice.rs`](examples/microservice.rs) | `/healthz` + `/readyz` probes, request ids, Prometheus metrics, `PORT` config, graceful shutdown |
| Public API for third parties | [`public_api.rs`](examples/public_api.rs) | API keys, per-key rate limiting with `retry-after`, CORS preflight, versioned group, pagination |
| SPA + backend in one binary | [`spa_backend.rs`](examples/spa_backend.rs) | Static frontend at `/`, JSON API under `/api`, security headers |
| Webhook receiver | [`webhook_receiver.rs`](examples/webhook_receiver.rs) | Constant-time token check, body limit, idempotency, `202` + background worker |
| Server-rendered HTML | [`html_forms.rs`](examples/html_forms.rs) | Form posts, cookie sessions, flash messages, Post/Redirect/Get, HTML escaping |

Run examples:

```bash
cargo run --example hello_world
cargo run --example microservice
PORT=8080 cargo run --example public_api   # scenario examples read PORT (default 3000)
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
- [x] Route groups, cookies, static file serving, body size limits (0.2)
- [x] Scenario examples, end-to-end HTTP benchmarks

### Next
- [ ] Group-level middleware (`RouteGroup::middleware` is accepted but ignored today)
- [ ] OpenAPI/Swagger generation
- [ ] WebSocket support
- [ ] Form data parsing helpers
- [ ] Rate limiting, compression and JWT helpers (patterns in `examples/public_api.rs`)
- [ ] Template engine integration

## CI/CD

GitHub Actions (`.github/workflows/ci.yml`):
- ✅ **Lint:** `rustfmt`, `clippy -D warnings`, rustdoc, `cargo audit` on every PR and on `main`
- ✅ **Test:** Linux, macOS, Windows
- ✅ **Build:** release build of all examples, benchmarks compile
- ✅ **Commits:** PR title and commits must be [Conventional Commits](https://www.conventionalcommits.org/)
- ✅ **Release:** on `main`, *only after lint, tests and build passed*, [release-plz](https://release-plz.dev) derives the next version from the commits, maintains a release PR with the changelog, and on merge tags, creates the GitHub release and publishes to crates.io

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
