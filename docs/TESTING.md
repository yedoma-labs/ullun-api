# Testing Guide

## Running Tests

### Unit Tests
```bash
cargo test --lib
```

### Integration Tests
```bash
cargo test --test integration_test
```

### All Tests
```bash
cargo test --all
cargo test --all-features    # jwt, compression and websocket tests
```

### Doc Tests
```bash
cargo test --doc
```

### With Coverage
```bash
cargo install cargo-tarpaulin
cargo tarpaulin --out Html --output-dir coverage
```

## Benchmarks

```bash
cargo bench                              # everything
cargo bench --bench router_benchmark     # micro-benchmarks
cargo bench --bench http_benchmark       # end-to-end over TCP
```

View results in `target/criterion/report/index.html`

## Test Structure

```
tests/
├── integration_test.rs   # Unit-level integration tests
├── e2e_test.rs           # End-to-end HTTP tests (fixed ports)
├── middleware_test.rs    # Middleware order, groups, CORS preflight, 404 wrapping, rate limiter,
│                         #   extensions; JWT and gzip middleware (features jwt, compression)
├── static_files_test.rs  # Static serving, index files, traversal (ephemeral port)
├── common/mod.rs         # spawn(app) + fluent HTTP client for tests
├── state_test.rs         # Shared application state
├── example_shortener.rs  # Real-world example apps, run over real HTTP
├── example_pastebin.rs   #   (the app modules are included with #[path])
├── example_taskboard.rs  #   (feature jwt)
├── example_chat.rs       #   (real WebSocket clients, features websocket + jwt)
├── example_gateway.rs    #   (real upstream servers, failover, streaming relay)
├── observability_test.rs # Request ids, access log, metrics
├── example_blog.rs       #   (browser-like: cookie jar, CSRF tokens scraped from HTML)
├── session_test.rs       # Sessions (timeouts, fixation, isolation) and CSRF
├── example_drive.rs      #   (multipart uploads, real files on disk)
├── example_statuspage.rs #   (real local targets, SSE, scheduler, shutdown)
├── routing_test.rs       # HEAD, OPTIONS, 405
├── files_test.rs         # serve_file, ranges, conditional requests, static handler hardening
├── body_limit_test.rs    # per-route body limits
├── conditional_test.rs   # ETag middleware and If-Match preconditions
├── streaming_test.rs     # Streaming bodies, SSE, disconnect detection
├── shutdown_test.rs      # Background tasks and graceful shutdown
├── openapi_test.rs       # Generated OpenAPI document and Swagger page
├── websocket_test.rs     # Handshake, echo, origin check, size limit (feature websocket)
└── pentest.rs            # Security regression tests

benches/
├── router_benchmark.rs   # Micro-benchmarks: routing, request/response, middleware chain
└── http_benchmark.rs     # End-to-end HTTP round trips over loopback
```

## Writing Tests

### Testing Handlers

```rust
use ullun::prelude::*;

#[tokio::test]
async fn test_handler() {
    let req = Request::new(
        http::Method::GET,
        "/test".parse().unwrap(),
        http::HeaderMap::new(),
        bytes::Bytes::new(),
    );
    
    let response = my_handler(req).await.unwrap();
    assert_eq!(response.status, http::StatusCode::OK);
}
```

### Testing Error Handling

```rust
#[test]
fn test_error_mapping() {
    let err = Error::bad_request("test");
    assert_eq!(err.status_code(), 400);
    assert_eq!(err.message(), "test");
}
```

### Testing example apps

The services in `examples/<name>/` keep their logic in `app.rs` (`pub fn build(config) -> App`) and only wire configuration in `main.rs`. Tests include the module and run it with a fake clock:

```rust
mod common;
#[allow(dead_code)]
#[path = "../examples/shortener/app.rs"]
mod shortener;

let server = common::spawn(shortener::build(config)).await;
let res = server.post("/api/links").header("x-api-key", "k").json(&body).send().await;
assert_eq!(res.status, 201);
```

### Testing against a real server

Bind to port `0` and use `App::serve` so tests never collide on ports:

```rust
let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
let addr = listener.local_addr().unwrap();
tokio::spawn(app.serve(listener));
// connect a hyper client to `addr`
```

See `tests/middleware_test.rs` for a complete helper.

### Testing Middleware

```rust
#[tokio::test]
async fn test_middleware() {
    async fn test_mw(req: Request, next: Next) -> Result<Response> {
        // Before
        let mut response = next.run(req).await?;
        // After
        response = response.with_header("X-Test", "value");
        Ok(response)
    }
    
    // Test middleware logic
}
```

## CI/CD Testing

Tests run automatically on:
- Pull requests to `main`
- Pushes to `main` (a release only follows when lint, tests and build all passed)
- Linux, macOS and Windows on stable Rust

See `.github/workflows/ci.yml`

## Performance Testing

### Load Testing with `wrk`

```bash
# Install wrk
brew install wrk  # macOS
sudo apt install wrk  # Linux

# Start server
cargo run --example hello_world --release

# Run load test
wrk -t12 -c400 -d30s http://localhost:3000/
```

Measured numbers and how to reproduce them: [PERFORMANCE.md](PERFORMANCE.md).

### Benchmarking Tips

1. **Always use `--release` builds**
2. **Warm up before measuring** (first few requests compile code)
3. **Test on production-like hardware**
4. **Monitor CPU and memory** during tests

## Test Coverage Goals

- **Unit tests:** >80% coverage
- **Integration tests:** All public APIs
- **Edge cases:** Error paths, invalid input
- **Security:** XSS, injection attempts

## Continuous Benchmarking

`cargo bench --no-run` runs in CI to keep the benchmarks compiling. Full runs are noisy on shared runners; run them locally and compare with `cargo bench -- --save-baseline before` / `--baseline before`.
