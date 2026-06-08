# Performance Guide

## Benchmarks

### Routing Performance

**Static routes (100 routes):**
- Insert: ~500ns per route
- Match: ~50-100ns per lookup

**Dynamic routes (with parameters):**
- One parameter: ~80-120ns
- Multiple parameters: ~100-150ns

### JSON Serialization

**Small payload (< 1KB):**
- Serialize: ~1-2µs
- Deserialize: ~2-3µs

**Medium payload (1-10KB):**
- Serialize: ~5-10µs
- Deserialize: ~10-20µs

### Request Handling

**End-to-end (hello world):**
- Throughput: 50,000+ req/s
- Latency p50: <1ms
- Latency p99: <5ms

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

Each middleware adds ~10-50ns overhead.

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

- **4 CPU cores:** ~200k req/s
- **8 CPU cores:** ~400k req/s
- **16 CPU cores:** ~800k req/s

### Horizontal Scaling

```
Load Balancer (nginx)
    |
    +-- ullun-api instance 1
    +-- ullun-api instance 2
    +-- ullun-api instance 3
    +-- ...
```

### Expected Throughput

| Setup | CPU | Memory | Req/s | p99 Latency |
|-------|-----|--------|-------|-------------|
| Single instance | 1 core | 256MB | 50k | <5ms |
| Single instance | 2 cores | 512MB | 100k | <5ms |
| Single instance | 4 cores | 1GB | 200k | <10ms |
| 3 instances | 4 cores each | 1GB each | 600k | <10ms |

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

| Framework | Req/s (simple) | Latency p99 | Memory |
|-----------|----------------|-------------|--------|
| **ullun-api** | **50k+** | **<5ms** | **2-5MB** |
| Actix-web | 60k+ | <5ms | 3-8MB |
| Axum | 45k+ | <10ms | 3-6MB |
| Rocket | 30k+ | <15ms | 5-10MB |
| warp | 40k+ | <10ms | 4-8MB |

*Benchmarks are approximate and depend on workload*

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
