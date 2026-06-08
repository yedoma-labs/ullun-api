# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-06-08

### Added
- **Route groups:** Organize routes with common prefixes using `App::group()`
- **Cookie support:** Parse cookies with `req.cookies()` and set with `response.cookie()`
- **Static file serving:** Serve files from directories with `serve_static()`
- **Request body size limits:** Configurable max body size with `max_body_size()` (default 10MB)
- Content-Type detection for 20+ file types
- Security: Directory traversal protection for static files
- SameSite cookie attribute support

### Changed
- Request struct now includes optional cookies field
- Response struct supports multiple Set-Cookie headers

### Security
- Added body size limit enforcement (prevents DoS attacks)
- Static file handler validates canonical paths

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
