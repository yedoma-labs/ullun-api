//! Scenario: token authentication for an API (feature `jwt`)
//!
//! - `POST /login` exchanges credentials for an HS256 token (15 minutes)
//! - `/api/*` requires `Authorization: Bearer <token>` (group middleware)
//! - handlers read the verified claims from the request extensions
//! - OpenAPI document with the bearer scheme and Swagger UI at `/docs`
//!
//! The user table is hard-coded; use a password hash (argon2) and a database.
//!
//! Port: set PORT (default 3000), JWT_SECRET (min. 32 bytes)
//! Run:  cargo run --example jwt_auth --features jwt
//! Try:  TOKEN=$(curl -s -X POST localhost:3000/login -H 'content-type: application/json' \
//!         -d '{"user":"ann","password":"demo"}' | sed 's/.*"token":"\([^"]*\)".*/\1/')
//!       curl -H "authorization: Bearer $TOKEN" localhost:3000/api/me
//!       curl -i -H "authorization: Bearer $TOKEN" localhost:3000/api/admin   # 403 unless admin

use serde_json::json;
use ullun::jwt::{self, Claims, JwtAuth};
use ullun::openapi::{schema, OpenApiInfo, Operation};
use ullun::prelude::*;

#[derive(Deserialize)]
struct Login {
    user: String,
    password: String,
}

async fn login(req: Request, secret: Vec<u8>) -> Result<Response> {
    let login: Login = req.json()?;
    // user -> role
    let role = match (login.user.as_str(), login.password.as_str()) {
        ("ann", "demo") => "admin",
        ("bob", "demo") => "member",
        _ => return Err(Error::unauthorized("wrong user or password")),
    };
    let token = jwt::encode(
        &json!({ "sub": login.user, "role": role, "exp": jwt::expires_in(15 * 60) }),
        &secret,
    )?;
    Ok(Response::json(json!({ "token": token, "expires_in": 900 })))
}

fn claims(req: &Request) -> Result<&Claims> {
    req.extensions
        .get::<Claims>()
        .ok_or_else(|| Error::internal("JwtAuth middleware missing"))
}

#[tokio::main]
async fn main() {
    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );
    let secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| {
        eprintln!("JWT_SECRET not set, using an insecure demo secret");
        "demo-secret-demo-secret-demo-secret".to_string()
    });
    let secret_for_login = secret.clone().into_bytes();

    App::new()
        .post("/login", move |req: Request| {
            let secret = secret_for_login.clone();
            async move { login(req, secret).await }
        })
        .doc(
            "POST",
            "/login",
            Operation::new("Log in")
                .tag("auth")
                .request_json(schema::object(
                    [("user", schema::string()), ("password", schema::string())],
                    ["user", "password"],
                ))
                .response_json(
                    200,
                    "Bearer token",
                    schema::object([("token", schema::string())], ["token"]),
                )
                .error_response(401, "Wrong credentials"),
        )
        .group("/api", |g| {
            g.middleware(JwtAuth::new(secret))
                .get("/me", |req: Request| async move {
                    Ok(Response::json(&claims(&req)?.0))
                })
                .doc(
                    "GET",
                    "/me",
                    Operation::new("Current user")
                        .tag("api")
                        .secured()
                        .error_response(401, "Missing, invalid or expired token"),
                )
                .get("/admin", |req: Request| async move {
                    if claims(&req)?.0["role"] != "admin" {
                        return Err(Error::forbidden("admin role required"));
                    }
                    Ok(Response::text("secret admin area"))
                })
                .doc(
                    "GET",
                    "/admin",
                    Operation::new("Admin area")
                        .tag("api")
                        .secured()
                        .error_response(403, "Not an admin"),
                )
        })
        .openapi(OpenApiInfo::new("Auth demo", "1.0.0").swagger_ui("/docs"))
        .run(&addr)
        .await
        .unwrap();
}
