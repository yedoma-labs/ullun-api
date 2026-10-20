//! Task board REST API (feature `jwt`)
//!
//! Users register and log in (argon2 password hashes, JWT bearer tokens), own
//! projects, and manage tasks in them: validation with per-field errors,
//! filtering, sorting and pagination, optimistic concurrency with ETag /
//! If-Match, ownership checks, login throttling and an OpenAPI document.
//!
//! Configuration (environment):
//!   PORT         listen port (default 3000)
//!   JWT_SECRET   token signing secret, at least 32 bytes (required unless DEV=1)
//!   DEV          set to 1 to run with an insecure built-in secret
//!
//! Run:   DEV=1 cargo run --example taskboard --features jwt
//! Try:   curl -s -X POST localhost:3000/auth/register -H 'content-type: application/json' \
//!             -d '{"email":"ann@example.com","password":"correct horse battery"}'
//!        TOKEN=$(curl -s -X POST localhost:3000/auth/login -H 'content-type: application/json' \
//!             -d '{"email":"ann@example.com","password":"correct horse battery"}' | sed 's/.*"token":"\([^"]*\)".*/\1/')
//!        curl -s -X POST localhost:3000/api/projects -H "authorization: Bearer $TOKEN" \
//!             -H 'content-type: application/json' -d '{"name":"Launch"}'
//!        open http://localhost:3000/docs for the generated API documentation

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let config = match app::Config::from_env() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("configuration error: {message}");
            std::process::exit(2);
        }
    };
    app::build(config)
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
