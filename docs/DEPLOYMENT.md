# Deployment Guide

## Building for Production

### Optimized Release Build

```bash
cargo build --release
```

Binary location: `target/release/your-app-name`

### Size Optimization

Add to `Cargo.toml`:

```toml
[profile.release]
opt-level = 'z'     # Optimize for size
lto = true          # Enable Link Time Optimization
codegen-units = 1   # Better optimization
strip = true        # Strip symbols
```

Expected binary size: ~1-2 MB (depending on your code)

## Docker Deployment

### Multi-Stage Dockerfile

```dockerfile
# Build stage
FROM rust:1.96 as builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
    
COPY --from=builder /app/target/release/your-app /usr/local/bin/app

EXPOSE 3000
CMD ["app"]
```

### Build and Run

```bash
docker build -t ullun-app .
docker run -p 3000:3000 ullun-app
```

## Environment Configuration

### Configuration File

```rust
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    host: String,
    port: u16,
    log_level: String,
}

fn load_config() -> Config {
    let config_str = std::fs::read_to_string("config.toml")
        .expect("Failed to read config");
    toml::from_str(&config_str).expect("Failed to parse config")
}

#[tokio::main]
async fn main() {
    let config = load_config();
    let addr = format!("{}:{}", config.host, config.port);
    
    App::new()
        // ... routes ...
        .run(&addr)
        .await
        .unwrap();
}
```

### Environment Variables

```rust
use std::env;

let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
let addr = format!("{}:{}", host, port);
```

## Production Best Practices

### 1. Logging

Add structured logging:

```rust
use tracing::{info, error};
use tracing_subscriber;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    
    info!("Starting server on {}", addr);
    
    App::new()
        .middleware(ullun::middleware::logger)
        // ... routes ...
        .run(&addr)
        .await
        .unwrap();
}
```

### 2. Graceful Shutdown

```rust
use tokio::signal;

async fn shutdown_signal() {
    signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C handler");
}

// In your run() implementation, handle shutdown gracefully
```

### 3. Health Checks

```rust
App::new()
    .get("/health", |_| async {
        Ok(Response::json(serde_json::json!({
            "status": "healthy",
            "timestamp": chrono::Utc::now().to_rfc3339()
        })))
    })
    .get("/ready", |_| async {
        // Check database, cache, etc.
        Ok(Response::json(serde_json::json!({"ready": true})))
    })
    // ... other routes ...
```

### 4. Error Handling

Don't expose internal errors in production:

```rust
async fn handler(req: Request) -> Result<Response> {
    database_operation()
        .await
        .map_err(|e| {
            eprintln!("Database error: {}", e);  // Log internally
            Error::internal("Service temporarily unavailable")  // Generic user message
        })
}
```

### 5. CORS

```rust
use ullun::middleware::cors;

App::new()
    .middleware(cors(vec![
        "https://yourdomain.com".to_string(),
        "https://api.yourdomain.com".to_string(),
    ]))
    // ... routes ...
```

## Deployment Platforms

### Fly.io

```toml
# fly.toml
app = "your-app-name"

[build]
  builder = "paketobuildpacks/builder:base"

[[services]]
  internal_port = 3000
  protocol = "tcp"

  [[services.ports]]
    handlers = ["http"]
    port = "80"

  [[services.ports]]
    handlers = ["tls", "http"]
    port = "443"
```

Deploy:
```bash
fly launch
fly deploy
```

### Railway

Create `railway.toml`:
```toml
[build]
builder = "NIXPACKS"

[deploy]
startCommand = "cargo run --release"
```

### DigitalOcean App Platform

Create `.do/app.yaml`:
```yaml
name: ullun-app
services:
  - name: api
    github:
      repo: your-username/your-repo
      branch: main
    build_command: cargo build --release
    run_command: ./target/release/your-app
    envs:
      - key: PORT
        value: "8080"
    http_port: 8080
```

### AWS Lambda

Use `cargo-lambda`:

```bash
cargo install cargo-lambda
cargo lambda build --release
cargo lambda deploy
```

### Kubernetes

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ullun-app
spec:
  replicas: 3
  selector:
    matchLabels:
      app: ullun-app
  template:
    metadata:
      labels:
        app: ullun-app
    spec:
      containers:
      - name: api
        image: your-registry/ullun-app:latest
        ports:
        - containerPort: 3000
        env:
        - name: HOST
          value: "0.0.0.0"
        - name: PORT
          value: "3000"
        resources:
          limits:
            memory: "128Mi"
            cpu: "500m"
---
apiVersion: v1
kind: Service
metadata:
  name: ullun-app
spec:
  selector:
    app: ullun-app
  ports:
  - port: 80
    targetPort: 3000
  type: LoadBalancer
```

## Monitoring

### Metrics

Add Prometheus metrics:

```rust
// Add prometheus crate
use prometheus::{Encoder, TextEncoder, Registry};

App::new()
    .get("/metrics", |_| async {
        let encoder = TextEncoder::new();
        let metric_families = prometheus::gather();
        let mut buffer = vec![];
        encoder.encode(&metric_families, &mut buffer).unwrap();
        
        Ok(Response::ok()
            .with_header("content-type", "text/plain")
            .with_body(buffer))
    })
    // ... routes ...
```

### Error Tracking

Integrate with Sentry:

```rust
sentry::init((
    "your-sentry-dsn",
    sentry::ClientOptions {
        release: sentry::release_name!(),
        ..Default::default()
    },
));

// Wrap handlers
async fn handler(req: Request) -> Result<Response> {
    sentry::configure_scope(|scope| {
        scope.set_tag("endpoint", req.uri.path());
    });
    
    // ... your logic
}
```

## Performance Tuning

### Connection Pooling

For database connections:

```rust
use sqlx::PgPool;

let pool = PgPool::connect(&database_url).await?;

// Share pool via Arc or state management
```

### Caching

Add Redis caching:

```rust
use redis::Client;

let client = Client::open("redis://127.0.0.1/")?;
let mut con = client.get_connection()?;
```

### Rate Limiting

Coming in future ullun-api release. For now, use:

```rust
// Custom middleware
use std::sync::Arc;
use tokio::sync::RwLock;

struct RateLimiter {
    // ... implementation
}

async fn rate_limit_middleware(req: Request, next: Next) -> Result<Response> {
    // Check rate limit
    // If exceeded, return 429
    next.run(req).await
}
```

## Security

### HTTPS

Use reverse proxy (nginx/Caddy) for TLS termination:

```nginx
server {
    listen 443 ssl http2;
    server_name api.yourdomain.com;
    
    ssl_certificate /path/to/cert.pem;
    ssl_certificate_key /path/to/key.pem;
    
    location / {
        proxy_pass http://localhost:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### Secrets Management

Never commit secrets. Use:
- Environment variables
- AWS Secrets Manager
- HashiCorp Vault
- Kubernetes Secrets

```rust
let db_password = env::var("DATABASE_PASSWORD")
    .expect("DATABASE_PASSWORD must be set");
```

## Troubleshooting

### High Memory Usage

- Profile with `valgrind` or `heaptrack`
- Check for memory leaks in handlers
- Reduce connection pool sizes

### High CPU Usage

- Profile with `perf` or `flamegraph`
- Check for blocking operations in async handlers
- Optimize hot paths

### Connection Issues

- Check firewall rules
- Verify port binding (0.0.0.0 vs 127.0.0.1)
- Test with `netstat` or `ss`

```bash
ss -tlnp | grep 3000
```

## Production Checklist

- [ ] Build with `--release`
- [ ] Enable size optimizations
- [ ] Configure logging
- [ ] Add health check endpoint
- [ ] Set up monitoring/metrics
- [ ] Configure error tracking
- [ ] Enable CORS if needed
- [ ] Use HTTPS (reverse proxy)
- [ ] Implement graceful shutdown
- [ ] Load test before launch
- [ ] Set up CI/CD
- [ ] Document deployment process
- [ ] Plan rollback strategy
