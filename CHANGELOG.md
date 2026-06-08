# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
