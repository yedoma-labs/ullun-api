# Performance Guide

## Benchmarks

Two [criterion](https://github.com/bheisler/criterion.rs) suites, both in `benches/`:

```bash
cargo bench --bench router_benchmark   # micro-benchmarks, no I/O
cargo bench --bench http_benchmark     # real TCP round trips, in-process server + hyper client
```

HTML reports land in `target/criterion/report/index.html`. Compare changes with
`cargo bench -- --save-baseline before` and `cargo bench -- --baseline before`.

### Reference results

Measured with ullun-api 0.2.0 + unreleased fixes, Rust 1.99.0 (`bench` profile, no LTO),
Intel Core i7-8750H (6C/12T) laptop, macOS 15.8, other apps running. Median of the
criterion confidence interval; treat them as order-of-magnitude, not as a promise.
Re-run on your own hardware before drawing conclusions.

**Routing** (`router_benchmark`, 100 static routes + 2 parameterised):

| Benchmark | Time |
|-----------|------|
| insert 10 routes | 5.3 µs (~0.5 µs/route) |
| insert 100 routes | 55 µs (~0.55 µs/route) |
| match static route | 125 ns |
| match 1 path param | 327 ns |
| match 2 path params | 485 ns |

A match includes the method dispatch, `Params` construction (owned `String`s) and the handler `Arc` clone, not only the `matchit` lookup.

**Request / response building:**

| Benchmark | Time |
|-----------|------|
| `Request::new` with 4 query params | 1.06 µs |
| `req.json::<T>()` (2 fields) | 225 ns |
| parse `Cookie` header (4 cookies) | 1.56 µs |
| `Response::text` | 515 ns |
| `Response::json` + `Set-Cookie` | 1.06 µs |
| `Response::from_error` | 971 ns |
| `Response::json` small / medium (3 users) | 0.90 µs / 4.1 µs |

**Middleware chain** (`Next::run`, async fn layers that only call `next`, plus final handler):

| Layers | Time | Per layer |
|--------|------|-----------|
| 0 | 867 ns | - |
| 1 | 1.07 µs | ~0.2 µs |
| 3 | 1.54 µs | ~0.22 µs |
| 10 | 3.27 µs | ~0.24 µs |

The 0-layer baseline includes building the `Request`. Every layer costs one `Arc` clone and one boxed future.

**End-to-end HTTP over loopback** (`http_benchmark`, keep-alive, `bench` profile, client and server share one 12-thread tokio runtime):

| Benchmark | Round trip |
|-----------|-----------|
| `GET /hello` (text) | 74 µs |
| `GET /users/{id}` (JSON) | 128-192 µs (noisy run) |
| `POST /users` (JSON echo) | 88 µs |
| `GET /nope` (404) | 68 µs |
| `GET /hello` + 3 middlewares | 70 µs |
| `POST /users` + 3 middlewares | 71 µs |
| 32 connections x 50 requests, `GET /hello` | **~71k req/s** (22.6 ms per 1600 requests) |

What this says: the framework's own work (routing, middleware, JSON) is in the low
microseconds and disappears behind the ~70 µs that syscalls, loopback TCP and the
hyper client cost; 3 middlewares are not measurable end-to-end. What it does not say:
the load generator runs on the same machine and competes for the same cores, so
real throughput on a dedicated server with an external generator (`wrk`, `hey`, see
below) will be higher. Latency percentiles (p50/p99) are not measured by criterion;
use `wrk --latency` for those.

### Binary size

`cargo build --release --example hello_world` produces a ~1.6 MB executable
(macOS x86_64, default release profile); add `lto = true`, `codegen-units = 1`,
`strip = true` to shrink it.

## Optimization Tips

### 1. Use Release Builds

```toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
```

### 2. Pre-compile Route Handlers

Routes are compiled at application startup, not per-request.

```rust
// ✅ Good - compiled once
App::new()
    .get("/users", list_users)
    .run("0.0.0.0:3000")
    .await
```

### 3. Minimize Middleware

Each middleware layer costs roughly 0.2 µs (one `Arc` clone + one boxed future), see the table above.

```rust
// Use only necessary middleware
App::new()
    .middleware(logger)  // Essential
    .middleware(auth)    // Needed
    // Skip unused middleware
```

### 4. Efficient JSON Handling

```rust
// ✅ Efficient - streaming deserialize
let user: User = req.json()?;

// ❌ Inefficient - double parse
let text = String::from_utf8(req.body.to_vec())?;
let user: User = serde_json::from_str(&text)?;
```

### 5. Connection Pooling

For database connections:

```rust
// Use connection pool
let pool = sqlx::PgPool::connect(&db_url).await?;

// Share via Arc
let pool = Arc::new(pool);
```

### 6. Response Caching

Cache frequently-accessed data:

```rust
use std::sync::Arc;
use tokio::sync::RwLock;

let cache = Arc::new(RwLock::new(HashMap::new()));

async fn handler(req: Request) -> Result<Response> {
    let cache = cache.read().await;
    if let Some(cached) = cache.get(&key) {
        return Ok(Response::json(cached));
    }
    // ... compute response
}
```

### 7. Async I/O

Always use async for I/O operations:

```rust
// ✅ Async I/O
let data = tokio::fs::read_to_string("file.txt").await?;

// ❌ Blocking I/O (blocks event loop)
let data = std::fs::read_to_string("file.txt")?;
```

## Load Testing

### Using `wrk`

```bash
# Simple load test
wrk -t4 -c100 -d30s http://localhost:3000/

# With latency histogram
wrk -t4 -c100 -d30s --latency http://localhost:3000/

# POST request
wrk -t4 -c100 -d30s -s post.lua http://localhost:3000/api/users
```

**post.lua:**
```lua
wrk.method = "POST"
wrk.body = '{"name":"Alice","email":"alice@example.com"}'
wrk.headers["Content-Type"] = "application/json"
```

### Using `hey`

```bash
# Install
go install github.com/rakyll/hey@latest

# Run test
hey -n 10000 -c 100 http://localhost:3000/
```

### Using `ab` (ApacheBench)

```bash
ab -n 10000 -c 100 http://localhost:3000/
```

## Production Optimization

### 1. Reverse Proxy

Use nginx/Caddy for:
- TLS termination
- Static file serving
- Load balancing
- Connection pooling

**nginx config:**
```nginx
upstream backend {
    least_conn;
    server 127.0.0.1:3000;
    server 127.0.0.1:3001;
    server 127.0.0.1:3002;
}

server {
    listen 443 ssl http2;
    
    location / {
        proxy_pass http://backend;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
    }
}
```

### 2. OS Tuning

**Increase file descriptors:**
```bash
# /etc/security/limits.conf
* soft nofile 65536
* hard nofile 65536
```

**TCP tuning:**
```bash
# /etc/sysctl.conf
net.ipv4.tcp_fin_timeout = 30
net.ipv4.tcp_tw_reuse = 1
net.core.somaxconn = 1024
```

### 3. Container Resources

**Docker:**
```yaml
services:
  api:
    image: ullun-api
    deploy:
      resources:
        limits:
          cpus: '2'
          memory: 512M
        reservations:
          cpus: '1'
          memory: 256M
```

**Kubernetes:**
```yaml
resources:
  requests:
    memory: "256Mi"
    cpu: "500m"
  limits:
    memory: "512Mi"
    cpu: "2000m"
```

### 4. Monitoring

Track key metrics:

```rust
use prometheus::{IntCounter, Histogram};

lazy_static! {
    static ref REQUEST_COUNT: IntCounter = 
        IntCounter::new("requests_total", "Total requests").unwrap();
    
    static ref REQUEST_DURATION: Histogram = 
        Histogram::new("request_duration_seconds", "Request duration").unwrap();
}

async fn monitored_handler(req: Request) -> Result<Response> {
    let start = Instant::now();
    REQUEST_COUNT.inc();
    
    let response = actual_handler(req).await;
    
    REQUEST_DURATION.observe(start.elapsed().as_secs_f64());
    response
}
```

## Scaling Strategies

### Vertical Scaling

The server is a tokio multi-thread runtime, so it uses all cores by default. Measure
your own handler mix with `wrk`/`hey` at 1, 2, 4... cores (`taskset`, container CPU limits)
before sizing; throughput depends mostly on what your handlers do (database, I/O).

### Horizontal Scaling

```
Load Balancer (nginx)
    |
    +-- ullun-api instance 1
    +-- ullun-api instance 2
    +-- ullun-api instance 3
    +-- ...
```

The framework keeps no cross-request state, so instances scale out as long as your own
state (sessions, rate-limit counters, idempotency keys) lives in a shared store.

## Profiling

### CPU Profiling

```bash
# Install flamegraph
cargo install flamegraph

# Profile application
cargo flamegraph --example hello_world
```

### Memory Profiling

```bash
# Install valgrind
sudo apt install valgrind

# Run with valgrind
valgrind --tool=massif target/release/examples/hello_world
```

### Async Profiling

```bash
# Use tokio-console
cargo install tokio-console

# Add to Cargo.toml
tokio = { version = "1", features = ["full", "tracing"] }
console-subscriber = "0.2"

# View console
tokio-console
```

## Comparison to Other Frameworks

No cross-framework numbers are published here. Comparable results need the same
hardware, load generator, handler and build profile for every framework; if you
run such a comparison, share the setup and open a PR.

## Performance Checklist

- [ ] Use release builds in production
- [ ] Enable LTO and strip symbols
- [ ] Minimize middleware layers
- [ ] Use connection pooling
- [ ] Cache frequently-accessed data
- [ ] Profile before optimizing
- [ ] Monitor metrics in production
- [ ] Use reverse proxy for TLS
- [ ] Tune OS limits
- [ ] Load test before launch
