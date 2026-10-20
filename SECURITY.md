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

**Sessions** - `ullun::session::SessionLayer`: unguessable 256-bit ids, `HttpOnly` + `SameSite=Lax` + `Secure` `__Host-` cookie, idle and absolute timeouts, no cookie for anonymous visitors. Call `session.renew()` on login (prevents fixation) and `session.destroy()` on logout. The default store is in memory; implement `SessionStore` for several instances.

**CSRF (Cross-Site Request Forgery)** - `ullun::csrf::Csrf` (after `SessionLayer`) requires the session token on `POST`/`PUT`/`PATCH`/`DELETE` (header or form field, constant-time compare) and rejects cross-site `Origin` / `Sec-Fetch-Site`. Bearer-token API clients can be exempted. Render the token only on pages that need it (creating it starts a session).

**Authentication/Authorization** - Building blocks only
- `jwt` feature: HS256 tokens and `JwtAuth` middleware (rejects `alg: none`, constant-time signature check, 32-byte minimum secret). Authorization (roles, ownership) stays in your handlers
- Hash passwords with a dedicated crate (argon2, bcrypt); never store them in plaintext

**Secrets and comparisons** - `ullun::security` provides CSPRNG tokens (`random_token`) and `constant_time_eq`; use them for session ids, API keys and delete tokens. Hash passwords with argon2 (see `examples/taskboard`).

**File uploads** - Never build a path from the client's file name: `multipart::sanitize_filename` is for display and download names, store under a generated id. Serve user content as `Content-Disposition: attachment` with `X-Content-Type-Options: nosniff` and a sandboxing CSP unless the type is known to be inert (images, PDF, media). Cap sizes per file and per user (`App::body_limit`, quotas), and write atomically. `examples/drive` shows all of this. `serve_static` never serves dotfiles and rejects encoded traversal.

**Server-side requests (SSRF)** - If your service fetches URLs that users or admins provide, use `ullun::security::resolve_public`: it requires every DNS answer to be a public address (loopback, private, link-local, CGNAT, multicast, documentation and IPv6 forms that embed IPv4 are refused) and returns the address to connect to, so check and use cannot diverge. `examples/statuspage` and `examples/webhooks` do this on every request.

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
4. **Basic CORS implementation** - Production should use more sophisticated origin checking
5. **CSRF protection is opt-in** - Add `SessionLayer` and `Csrf` to every app that uses cookie authentication

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
