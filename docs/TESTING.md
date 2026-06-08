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
cargo bench
```

View results in `target/criterion/report/index.html`

## Test Structure

```
tests/
├── integration_test.rs   # Unit-level integration tests
└── e2e_test.rs          # End-to-end HTTP tests (requires ports)

benches/
└── router_benchmark.rs  # Performance benchmarks
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
- Pull requests
- Pushes to main/develop
- Multiple OS (Linux, macOS, Windows)
- Multiple Rust versions (stable, beta)

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

### Expected Performance

- **Throughput:** 50,000+ requests/second (hello world)
- **Latency (p50):** <1ms
- **Latency (p99):** <5ms
- **Memory:** ~2-5MB per connection

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

Benchmarks run on every push to main and results are tracked over time.

View trends: https://github.com/yedoma-labs/ullun-api/benchmarks
