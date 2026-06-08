# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/yedoma-labs/ullun-api/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/yedoma-labs/ullun-api/releases/tag/v0.1.0
