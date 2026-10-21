# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Security Considerations

### Framework-Level Protections

✅ **Memory Safety**
- No `unsafe` code in framework
- All Rust safety guarantees apply
- Bounds-checked array access

✅ **Input Validation**
- Path parameters validated by matchit router
- JSON parsing errors return 400 Bad Request
- Query parameter parsing is non-panicking
- Header validation prevents malformed headers

✅ **Error Handling**
- No unwrap() in production code paths
- All errors mapped to appropriate HTTP status codes
- Internal errors don't leak sensitive information

⚠️ **User Responsibilities**

**SQL Injection** - Framework doesn't provide database access
- Use parameterized queries with sqlx/diesel
- Never concatenate user input into SQL strings

**XSS (Cross-Site Scripting)** - Framework serves JSON by default
- If serving HTML, sanitize all user input
- Use templating engines with auto-escaping (Tera, Askama)
- Set Content-Security-Policy headers

**CORS** - `ullun::cors::Cors` matches the request's `Origin` exactly against your list (or a predicate), echoes it with `Vary: Origin`, answers preflights itself (403 for unknown origins or unlisted methods), adds the headers to error responses too, and never sends credentials together with a wildcard origin. The old `middleware::cors` function is deprecated and delegates to it.

**Security headers** - `ullun::security_headers::SecurityHeaders` sets `nosniff`, `X-Frame-Options`, `Referrer-Policy`, COOP/CORP and a restrictive `Permissions-Policy` on every response, including errors and 404s, without overriding headers a handler set. `Csp` builds a Content-Security-Policy (`Csp::api()`, `Csp::web()`, report-only mode) and supports a fresh per-request nonce through the `{nonce}` placeholder and `Request::csp_nonce()`.

**Sessions** - `ullun::session::SessionLayer`: unguessable 256-bit ids, `HttpOnly` + `SameSite=Lax` + `Secure` `__Host-` cookie, idle and absolute timeouts, no cookie for anonymous visitors. Call `session.renew()` on login (prevents fixation) and `session.destroy()` on logout. The default store is in memory; implement `SessionStore` for several instances.

**CSRF (Cross-Site Request Forgery)** - `ullun::csrf::Csrf` (after `SessionLayer`) requires the session token on `POST`/`PUT`/`PATCH`/`DELETE` (header or form field, constant-time compare) and rejects cross-site `Origin` / `Sec-Fetch-Site`. Bearer-token API clients can be exempted. Render the token only on pages that need it (creating it starts a session).

**Authentication/Authorization** - Building blocks only
- `jwt` feature: HS256 tokens and `JwtAuth` middleware (rejects `alg: none`, constant-time signature check, 32-byte minimum secret). Authorization (roles, ownership) stays in your handlers
- Hash passwords with a dedicated crate (argon2, bcrypt); never store them in plaintext

**Secrets and comparisons** - `ullun::security` provides CSPRNG tokens (`random_token`) and `constant_time_eq`; use them for session ids, API keys and delete tokens. Hash passwords with argon2 (see `examples/taskboard`).

**File uploads** - Never build a path from the client's file name: `multipart::sanitize_filename` is for display and download names, store under a generated id. Serve user content as `Content-Disposition: attachment` with `X-Content-Type-Options: nosniff` and a sandboxing CSP unless the type is known to be inert (images, PDF, media). Cap sizes per file and per user (`App::body_limit`, quotas), and write atomically. `examples/drive` shows all of this. `serve_static` never serves dotfiles and rejects encoded traversal.

**Server-side requests (SSRF)** - If your service fetches URLs that users or admins provide, use `ullun::security::resolve_public`: it requires every DNS answer to be a public address (loopback, private, link-local, CGNAT, multicast, documentation and IPv6 forms that embed IPv4 are refused) and returns the address to connect to, so check and use cannot diverge. `examples/statuspage` and `examples/webhooks` do this on every request.

**Streaming uploads** - `App::stream_body(prefix, limit)` skips body buffering for large uploads; the limit is enforced while the bytes arrive (an announced `Content-Length` over the limit is refused before reading) and `BodyStream::save_to` removes partial files when an upload fails or is cut off. The stream moves bytes only: sanitize file names and target paths yourself, and combine it with authentication and a rate limit, because a streaming route has no buffer cap other than the one you set.

**Outbound requests (SSRF)** - Whenever a URL comes from a user, build the client with `HttpClient::public_only()`: every resolved address must be public (loopback, private, link-local and cloud metadata ranges are refused), the connection goes to the vetted address itself, and redirects are re-checked per hop. Redirects drop `Authorization` and `Cookie` when they leave the origin, responses are size-capped, and retries only repeat requests that are safe to repeat (idempotent methods or an `Idempotency-Key`).

**Response cache** - `ullun::cache::ResponseCache` never stores responses to requests carrying `Authorization` or `Cookie` (unless you opt in with `per_user()`, which keys entries by credential), nor responses with `Set-Cookie`, `Cache-Control: no-store` / `private` / `no-cache`, streams or oversized bodies. Client `Cache-Control` request headers are ignored, so nobody can force refreshes. Use `vary_by` for every request header that changes the body, or one variant is served to everybody.

**Idempotency** - `ullun::idempotency::Idempotency` makes retried `POST`/`PATCH` safe: the handler runs once per `Idempotency-Key`, retries get the stored response (`Idempotency-Replayed: true`), a concurrent duplicate gets `409`, a reused key with a different payload `422`. Keys are scoped to the caller (`Authorization` + `Cookie` by default, override with `scope_by`) so nobody can replay another user's response. `5xx` and streaming responses are not stored. The default store is process-local; implement `IdempotencyStore` for several instances.

**Resource limits** - `App::header_read_timeout` (default 30 s) cuts clients that trickle request headers or idle on keep-alive (slowloris); `App::max_connections` and `max_connections_per_ip` refuse excess connections with an immediate `503`; `limits::Timeout` answers `504` for slow handlers; `limits::ConcurrencyLimit` sheds load with `503` + `Retry-After` (optionally queueing briefly) instead of building a backlog. Body size is capped by `max_body_size` / `body_limit`. Timeout and concurrency limit cover the time until the handler returns a `Response`, not a streaming body. Behind a reverse proxy do not use the per-IP cap (all connections share the proxy's IP).

**TLS** - With the `tls` feature the server speaks HTTPS itself (`App::tls`), HTTP/2 included (ALPN `h2`; stream concurrency is capped at 128 per connection and hyper's/h2's reset-flood mitigations apply): TLS 1.2/1.3 only, certificate hot-reload that keeps the old certificate when a renewal is broken, handshake timeout, optional or required client certificates (`TlsConfig::client_auth`). A client certificate proves membership of your CA, not authorization: check its fingerprint or subject in the handler (see `examples/secure_api`). Add `secure::Hsts`, and redirect plain HTTP with `secure::RequireHttps` (the redirect target is built from a strictly validated `Host`; set `allowed_hosts` in production). Behind a reverse proxy, believe `X-Forwarded-Proto` only if the proxy sets it.

**Rate Limiting** - `RateLimiter` middleware (per instance, in memory)
- Keyed by peer IP by default; `trust_forwarded_for()` only behind your own reverse proxy
- Use the reverse proxy (nginx, Caddy) or a shared store for limits across instances

**WebSockets** (`websocket` feature) - Cross-site origins are rejected by default (browsers do not apply the same-origin policy to WebSockets), messages are capped at 1 MiB. Authenticate connections in middleware or in the first message

**File Uploads** - Not supported in v0.1
- If added, validate file types
- Limit file sizes
- Scan for malware
- Store outside webroot

### Best Practices

1. **Always use HTTPS in production**
   ```nginx
   # Use reverse proxy for TLS
   server {
       listen 443 ssl http2;
       ssl_certificate /path/to/cert.pem;
       ssl_certificate_key /path/to/key.pem;
       
       location / {
           proxy_pass http://localhost:3000;
       }
   }
   ```

2. **Validate and sanitize all input**
   ```rust
   async fn create_user(req: Request) -> Result<Response> {
       let body: CreateUserRequest = req.json()?;
       
       // Validate
       if body.email.len() > 255 {
           return Err(Error::bad_request("Email too long"));
       }
       
       // Sanitize before using
       let email = body.email.trim().to_lowercase();
       // ...
   }
   ```

3. **Use environment variables for secrets**
   ```rust
   let db_password = env::var("DATABASE_PASSWORD")
       .expect("DATABASE_PASSWORD must be set");
   ```

4. **Set security headers**
   ```rust
   async fn security_headers(req: Request, next: Next) -> Result<Response> {
       let mut response = next.run(req).await?;
       response = response
           .with_header("X-Content-Type-Options", "nosniff")
           .with_header("X-Frame-Options", "DENY")
           .with_header("X-XSS-Protection", "1; mode=block")
           .with_header("Strict-Transport-Security", "max-age=31536000");
       Ok(response)
   }
   ```

5. **Log security events**
   ```rust
   if auth_failed {
       eprintln!("Failed login attempt for user: {}", username);
   }
   ```

6. **Keep dependencies updated**
   ```bash
   cargo update
   cargo audit
   ```

## Reporting a Vulnerability

**DO NOT** open a public issue for security vulnerabilities.

Please report security vulnerabilities privately:

1. Email: security@yedoma-labs.com (if available)
2. Open a GitHub Security Advisory
3. Contact maintainers directly

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will:
- Acknowledge within 48 hours
- Provide a fix timeline
- Credit you in release notes (unless you prefer anonymity)
- Publish a security advisory after fix is released

## Security Checklist for Deployments

- [ ] HTTPS enabled (TLS 1.2+)
- [ ] Security headers configured
- [ ] Secrets in environment variables
- [ ] Input validation on all endpoints
- [ ] Authentication/authorization middleware
- [ ] Rate limiting configured
- [ ] Logging enabled
- [ ] Dependencies audited (`cargo audit`)
- [ ] Error messages don't leak sensitive info
- [ ] CORS configured appropriately
- [ ] Database queries parameterized
- [ ] File permissions restrictive
- [ ] Reverse proxy configured
- [ ] Monitoring/alerting set up

## Known Limitations

1. **Authentication is opt-in** - Use `JwtAuth` (feature `jwt`) or your own middleware
2. **Rate limiting is per instance** - See `ullun::rate_limit`; use a proxy or shared store for global limits
3. **Request size limits configurable** - Default 10MB, configure with `max_body_size()` (v0.2.0+)
4. **CSRF protection is opt-in** - Add `SessionLayer` and `Csrf` to every app that uses cookie authentication

## Security Roadmap

Future versions will include:

- [x] Rate limiting middleware (`ullun::rate_limit`)
- [x] Request size limits (`App::max_body_size`, `App::body_limit`)
- [x] JWT authentication helpers (`jwt` feature)
- [x] Sessions and CSRF token middleware (`ullun::session`, `ullun::csrf`)
- [x] OpenAPI security scheme generation (bearer and API key)
- [ ] Security headers middleware (helmet-style; see `examples/blog`)
- [ ] Audit logging framework

## References

- [OWASP Top 10](https://owasp.org/www-project-top-ten/)
- [Rust Security Guidelines](https://anssi-fr.github.io/rust-guide/)
- [CWE Top 25](https://cwe.mitre.org/top25/)
