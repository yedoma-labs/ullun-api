# ullun-api Architecture

## Principles

1. **Simple surface** - an Express-style builder (`App::new().get(..).middleware(..).run(..)`); everything else is an opt-in module.
2. **Thin core on hyper/tokio** - the core adds routing, middleware and conversions, not a runtime of its own.
3. **Safe defaults** - limits, timeouts and header hygiene are on or one call away; unsafe code is not used.
4. **Few dependencies** - the core needs `hyper`, `tokio`, `matchit`, `serde`; heavier things (`rustls`, `hmac`, `flate2`, WebSocket) sit behind cargo features.
5. **Testable without sockets** - the whole request pipeline can run in-process (`testing::TestClient`).

## Layers

```
                 ┌──────────────────────────────────────────────────────────┐
 application     │ handlers, App builder, route groups                      │
                 └──────────────────────────────────────────────────────────┘
                 ┌──────────────────────────────────────────────────────────┐
 middleware      │ cors, security_headers, session, csrf, jwt, rate_limit,  │
 (opt-in)        │ limits, idempotency, cache, compression, conditional,    │
                 │ observability, trace, problem, secure                    │
                 └──────────────────────────────────────────────────────────┘
                 ┌──────────────────────────────────────────────────────────┐
 core            │ app, router, handler, middleware, request, response,     │
                 │ error, cookies, upload, stream, shutdown                 │
                 └──────────────────────────────────────────────────────────┘
                 ┌──────────────────────────────────────────────────────────┐
 transport       │ hyper (HTTP/1.1, HTTP/2), tokio, tls (rustls)            │
                 └──────────────────────────────────────────────────────────┘
```

Modules above the core never reach into `app.rs`; they talk to the framework only through `Middleware`, `Handler`, `Request` and `Response`. That keeps them replaceable and individually testable.

## Module map

| Area | Modules |
|------|---------|
| Core | `app` (builder, accept loop, connection driving), `router` (matchit per method), `handler`, `middleware` (`Middleware`, `Next`, `Layered`), `request`, `response`, `error`, `cookies`, `shutdown` |
| Request bodies | `upload` (streaming), `multipart`, `validate` (field errors → 422) |
| Responses | `sse`, `stream`, `files` (ranges, conditional), `static_files` (precompressed, SPA), `template` |
| Security | `cors`, `security_headers`, `session`, `csrf`, `jwt`, `signed_cookies`, `security` (tokens, SSRF checks), `secure` (HTTPS policy), `tls`, `rate_limit` |
| Reliability | `limits` (timeouts, concurrency, connection caps), `health`, `idempotency`, `cache`, `jobs`, `hub`, `compression`, `conditional` |
| Observability | `observability` (request id, access log, metrics), `trace` (W3C trace context) |
| API contract | `openapi`, `problem` (RFC 9457), `negotiate` |
| Outbound / tests | `client` (HTTP client), `testing` (in-process `TestClient`) |

Optional dependencies are behind features: `jwt`/`signing` (hmac, sha2, base64), `compression` (flate2), `websocket`, `tls` and `client-tls` (rustls, webpki-roots).

## Request flow

```
TCP accept (app.rs)
  ├─ ConnLimits.admit()        total / per-IP caps → 503 and close
  ├─ TLS handshake (tls feature, own task, timeout) → ALPN: h2 | http/1.1
  └─ serve_connection
       ├─ http1::Builder  (header_read_timeout, upgrades for WebSocket)
       └─ http2::Builder  (stream cap, keep-alive pings)
            │
            ▼
       handle_request(hyper::Request<Incoming>)
         ├─ body: buffered up to max_body_size / body_limit, or left unread for stream_body routes
         ├─ :authority → Host header (HTTP/2)
         └─ run_pipeline(Request)                       ← shared with TestClient
              ├─ Router::match_route_with_pattern → Found(handler) | Miss(allow)
              ├─ middleware chain, in registration order
              │    (global middleware first, then group middleware, then the handler)
              ├─ Err(e) → Response::from_error
              └─ HEAD → headers only
            │
            ▼
       Response::into_hyper → written by hyper
```

Everything between `handle_request` and `into_hyper` is plain data in, plain data out, which is why `TestClient` can call `run_pipeline` directly.

### Middleware order

Middleware wraps from the outside in: the first registered runs first on the way in and last on the way out. A sensible order:

1. `TraceLayer`, `RequestIdLayer`, `AccessLog`, `Metrics` - see everything, including rejections
2. `SecurityHeaders`, `Cors` - decorate every response, including errors and preflights
3. `ProblemDetails` - convert error bodies produced further in
4. `Timeout`, `ConcurrencyLimit`, `RateLimiter` - shed load before doing work
5. `SessionLayer`, `Csrf`, `JwtAuth` - identity
6. `Idempotency`, `ResponseCache`, `Compression`, `ETags` - per-route behaviour (use groups to scope them)

Middleware also runs for unmatched routes (404/405/OPTIONS), so CORS preflights and request ids work without a route.

## Extension points

| Trait | Purpose | Default |
|-------|---------|---------|
| `Handler` | anything that answers a request; implemented for async closures and `async fn(Request)` | - |
| `Middleware` | `(Request, Next) → Result<Response>`; implemented for async closures | - |
| `SessionStore` | where session data lives | in-memory |
| `IdempotencyStore` | where idempotency keys and replays live | in-memory |
| `TemplateEngine` | rendering | built-in engine |

In-memory defaults are bounded (entry caps) and process-local; implement the trait for Redis or SQL to share state between instances.

## Error handling

Handlers return `Result<Response, Error>`. `Error` maps to a status code (`BadRequest` 400 … `Validation` 422, `Custom(status, msg)`), and `Response::from_error` renders `{"error", "status"}` JSON (plus `fields` for validation). `ProblemDetails` rewrites that into RFC 9457. `ClientError` (outbound client) converts into `Error` with 502/504, so `?` works across service calls. A panic in a handler is caught by tokio's task boundary and ends only that connection.

## Concurrency model

- One tokio task per connection; HTTP/2 multiplexes requests on it.
- Shared state is `Arc`; handlers are `Send + Sync` and take `Request` by value.
- Background work (`App::background`, `JobQueue`) gets a `Shutdown` signal; graceful shutdown stops accepting, drains requests and tasks for `shutdown_grace`, after an optional `shutdown_delay` during which readiness reports "draining".
- Blocking or CPU-heavy work belongs in `spawn_blocking`; the framework never blocks the runtime itself.

## Testing strategy

| Level | Tool |
|-------|------|
| Units | `#[cfg(test)]` in each module (parsers, stores, math) |
| Pipeline | `testing::TestClient` - routing, middleware, cookies, no sockets |
| Wire | `tests/common` spawns an `App` on port 0 and talks real HTTP (limits, timeouts, TLS, HTTP/2, WebSockets, streaming) |
| Examples | every `examples/<name>` has `tests/example_<name>.rs` that includes its `app.rs` |
| Performance | `benches/` (criterion) and `bench/` (cross-framework load test, see PERFORMANCE.md) |

## Design decisions

- **matchit** for routing: radix tree, parameters, catch-alls, maintained, used widely.
- **Runtime state over proc macros**: routes are registered with plain method calls; compile times stay low and nothing is hidden.
- **`Request` owns its body**: simple handlers by default (`req.json()`), with an explicit opt-in for streaming where memory matters.
- **Middleware as a trait object chain** (`Arc<Vec<Arc<dyn Middleware>>>` plus an index): one allocation-free `Arc` clone and one boxed future per layer, measured at roughly 0.2 µs.
- **Own small HTTP client** instead of a re-export: it shares the SSRF guard, tracing and error types with the server side.
- **Deliberately not included**: an ORM, a config system, background-job persistence beyond the in-process queue, and an OpenTelemetry exporter.
