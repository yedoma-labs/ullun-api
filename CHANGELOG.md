# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.1](https://github.com/yedoma-labs/ullun-api/compare/v0.3.0...v0.3.1) - 2026-10-20

### Added

- *(examples)* add a status page and uptime monitor
- *(app)* add background tasks and graceful shutdown
- *(response)* stream response bodies and add Server-Sent Events

### Other

- *(examples)* accept a timeout for refused connections on Windows
- document streaming, SSE, background tasks and graceful shutdown

## [0.3.0](https://github.com/yedoma-labs/ullun-api/compare/v0.2.3...v0.3.0) - 2026-10-20

### Added

- *(examples)* add a task board REST API
- *(conditional)* add ETag middleware and If-Match preconditions
- *(request)* parse typed path parameters
- *(error)* [**breaking**] add structured validation errors

### Other

- document validation, typed path parameters and conditional requests

## [0.2.3](https://github.com/yedoma-labs/ullun-api/compare/v0.2.2...v0.2.3) - 2026-10-20

### Added

- *(examples)* add a pastebin service
- *(middleware)* implement Middleware for Arc<M>
- *(examples)* add a URL shortener service
- *(openapi)* document API key security
- *(security)* add random tokens and constant-time comparison
- *(app)* add shared application state

### Other

- document shared state, security helpers and the real-world examples

## [0.2.2](https://github.com/yedoma-labs/ullun-api/compare/v0.2.1...v0.2.2) - 2026-10-20

### Added

- *(examples)* add jwt, websocket and template scenarios
- *(openapi)* generate an OpenAPI 3.1 document from registered routes
- *(websocket)* add WebSocket endpoints behind the websocket feature
- *(compression)* add gzip compression middleware behind the compression feature
- *(jwt)* add HS256 JWT helpers behind the jwt feature
- *(rate-limit)* add rate limiting middleware
- *(template)* add template engine integration
- *(request)* parse urlencoded form bodies
- *(request)* expose extensions and peer address
- *(app)* support middleware on route groups

### Other

- document the new features and update the roadmap

## [0.2.1](https://github.com/yedoma-labs/ullun-api/compare/v0.2.0...v0.2.1) - 2026-10-20

### Added

- *(examples)* add scenario examples
- *(app)* add App::serve for pre-bound listeners

### Fixed

- *(middleware)* run middleware for unmatched routes
- *(static)* serve files via the matchit 0.9 catch-all route

### Other

- document scenario examples, measured benchmarks and release flow
- lint, test and build gate with release-plz releases
- *(bench)* add end-to-end HTTP and request/middleware benchmarks
- *(deps)* update dependencies to latest versions
- apply rustfmt
- adds header image

## [0.2.0] - 2026-06-08

### Added
- **Route groups:** Organize routes with common prefixes using `App::group()`
  - Group-level route registration
  - Clean API organization
  - Example: `.group("/api", |g| g.get("/users", handler))`
  
- **Cookie support:** Full HTTP cookie handling
  - Parse cookies with `req.cookies()`
  - Set cookies with `response.cookie()`
  - Cookie attributes: domain, path, max-age, secure, httponly, samesite
  - Type-safe SameSite enum (Strict, Lax, None)
  - RFC 6265bis compliance
  
- **Static file serving:** Production-ready file server
  - `serve_static("/prefix/", "directory")` method
  - Content-Type detection for 20+ file types
  - Directory traversal protection (canonical path validation)
  - Index.html fallback for directories
  - Cache-Control headers
  
- **Request body size limits:** DoS prevention
  - Default 10MB limit
  - Configurable with `max_body_size(Some(bytes))`
  - Enforced during streaming (prevents bypass)
  - Returns 413 Payload Too Large

### Changed
- Request struct uses `OnceCell` for lazy cookie parsing
- `req.cookies()` now takes `&self` instead of `&mut self`
- Response struct supports multiple Set-Cookie headers

### Security
- **[CRITICAL]** Fixed TOCTOU race in static file path traversal check
- **[CRITICAL]** Enforced body size limit during streaming (prevents chunked bypass)
- **[CRITICAL]** Added cookie value validation (prevents header injection)
- **[HIGH]** Auto-enable Secure flag for SameSite=None cookies
- **[HIGH]** Static file handler validates canonical paths before serving
- Cookie names/values validated against forbidden characters (`;`, `=`, control chars)

### Fixed
- Removed broken group-level middleware (would apply globally)
- Fixed redundant local variable warnings

### Breaking Changes
- None - fully backward compatible with 0.1.0

## [0.1.0] - 2026-06-08

### Added
- Initial release
- Express.js-style routing with `App::new()`
- HTTP method handlers: `get()`, `post()`, `put()`, `delete()`, `patch()`
- Path parameter extraction with `get_with_params()`
- Middleware support with `middleware()`
- Built-in middleware: `logger`, `cors`
- JSON request/response helpers
- Error handling with automatic HTTP status code mapping
- Type-safe `Result<T, Error>` pattern
- Query parameter parsing
- Request body parsing (`req.json()`)
- Comprehensive examples and documentation

### Technical Details
- Built on hyper 1.10 + tokio 1.52
- Uses matchit 0.9 for fast routing
- ~150KB compiled binary size
- Zero unsafe code

[Unreleased]: https://github.com/yedoma-labs/ullun-api/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/yedoma-labs/ullun-api/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/yedoma-labs/ullun-api/releases/tag/v0.1.0
