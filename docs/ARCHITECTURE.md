# ullun-api Architecture

## Overview

ullun-api is built on three core principles:
1. **Simplicity** - Express.js-style API
2. **Performance** - Zero-cost abstractions on hyper/tokio
3. **Type Safety** - Compile-time guarantees

## Module Structure

```
src/
├── lib.rs         # Public API and prelude
├── app.rs         # Main application orchestrator
├── router.rs      # HTTP routing (matchit-based)
├── handler.rs     # Handler traits and wrappers
├── middleware.rs  # Middleware chain
├── request.rs     # Request types and extractors
├── response.rs    # Response builders
└── error.rs       # Error types and Result alias
```

## Request Flow

```
TCP Connection
    │
    ├─> hyper HTTP/1 Server
    │
    ├─> App::handle_request()
    │   ├─> Parse request body
    │   ├─> Router::match_route()
    │   │   └─> matchit::Router<DynHandler>
    │   │
    │   ├─> Middleware Chain
    │   │   ├─> Middleware 1
    │   │   ├─> Middleware 2
    │   │   └─> Handler
    │   │
    │   └─> Response::into_hyper()
    │
    └─> Send HTTP response
```

## Key Components

### App

The main orchestrator. Uses builder pattern to configure routes and middleware before running.

```rust
pub struct App {
    router: Router,              // Routing table
    middlewares: Vec<Arc<dyn Middleware>>,  // Middleware stack
}
```

During `.run()`, Router and middlewares are wrapped in `Arc` for shared access across async tasks.

### Router

Fast path-based routing using the `matchit` library.

```rust
pub struct Router {
    get: matchit::Router<DynHandler>,
    post: matchit::Router<DynHandler>,
    // ... other methods
}
```

- Separate router per HTTP method for O(1) method dispatch
- matchit provides fast path matching with parameter extraction
- Handlers are type-erased (`Arc<dyn Handler>`) for flexibility

### Handler

Trait for request handlers. Supports both closures and async functions.

```rust
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn call(&self, req: Request) -> Result<Response>;
}
```

Automatically implemented for:
- `Fn(Request) -> Future<Output = Result<Response>>`
- Custom wrapper for path param extraction

### Middleware

Inspired by Express.js middleware pattern.

```rust
#[async_trait]
pub trait Middleware: Send + Sync + 'static {
    async fn handle(&self, req: Request, next: Next) -> Result<Response>;
}
```

Middleware can:
- Modify request before handler
- Short-circuit with early response
- Modify response after handler
- Async I/O (logging, auth checks, etc.)

### Request/Response

Type-safe wrappers around hyper types.

**Request:**
- Path parameters (`Params`)
- Query parameters (`Query`)
- JSON body parsing
- Headers, method, URI

**Response:**
- Builder pattern for construction
- JSON helper
- Error→Response conversion

## Type Safety

### Path Parameters

Path parameters are extracted at runtime but validated through the type system:

```rust
.get_with_params("/users/{id}", |params| async move {
    let id: u64 = params.get("id")?.parse()
        .map_err(|_| Error::bad_request("Invalid ID"))?;
    // ...
})
```

- `params.get()` returns `Result<&str, Error>`
- Missing params → automatic 400 error
- Type conversion failures caught and handled

### Error Handling

All handlers return `Result<Response, Error>`:

```rust
pub enum Error {
    BadRequest(String),    // 400
    Unauthorized(String),  // 401
    Forbidden(String),     // 403
    NotFound(String),      // 404
    Internal(String),      // 500
    Custom(u16, String),
}
```

Errors automatically convert to appropriate HTTP responses with JSON body.

## Performance Characteristics

### Routing

- **Method dispatch:** O(1) hash map lookup
- **Path matching:** O(log n) via radix tree (matchit)
- **Parameter extraction:** O(k) where k = number of params

### Memory

- **Handler storage:** Arc pointer per route (~8 bytes)
- **Middleware:** Arc pointer per middleware (~8 bytes)
- **Router:** Radix tree, ~32 bytes per node
- **Request/Response:** Stack-allocated where possible

### Concurrency

- **Connection handling:** One tokio task per connection
- **Shared state:** Arc for zero-copy sharing
- **Thread safety:** All handlers are `Send + Sync`

## Future Optimizations

1. **Handler specialization:** Reduce trait object overhead
2. **Request pooling:** Reuse Request allocations
3. **Zero-copy body:** Stream large bodies without buffering
4. **Compile-time routing:** Macro-based route generation

## Design Decisions

### Why matchit over custom router?

- Battle-tested in production
- Fast radix tree implementation
- Active maintenance
- Similar to Go's httprouter

### Why not proc macros for routing?

- Simpler to understand and debug
- Faster compile times
- More flexible at runtime
- Can add later without breaking changes

### Why Builder pattern over macros?

- Familiar to Rust developers
- Clear ownership and lifetimes
- No hidden magic
- Better IDE support

### Why Arc<dyn Trait> over generics?

- Simpler API surface
- No generic explosion
- Dynamic dispatch overhead is negligible for I/O-bound handlers
- Easier to extend

## Testing Strategy

- **Unit tests:** Each module
- **Integration tests:** Full HTTP round-trips
- **Examples:** Serve as smoke tests
- **No mocking:** Test against real hyper server

## Dependencies

Chosen for stability and minimalism:

- `hyper` - HTTP server foundation
- `tokio` - Async runtime
- `matchit` - Fast routing
- `serde/serde_json` - JSON handling
- `async-trait` - Async trait syntax
- `thiserror` - Error handling

All dependencies are mature, actively maintained, and have minimal transitive dependencies.
