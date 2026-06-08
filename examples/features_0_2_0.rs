//! Example showcasing v0.2.0 features:
//! - Route groups
//! - Cookie handling
//! - Static file serving

use ullun::cookies::SameSite;
use ullun::prelude::*;

#[tokio::main]
async fn main() {
    // Create static directory for demo
    std::fs::create_dir_all("static").ok();
    std::fs::write("static/index.html", "<h1>Welcome to ullun-api!</h1>").ok();
    std::fs::write("static/style.css", "body { font-family: sans-serif; }").ok();

    App::new()
        // Set max request body size to 5 MB
        .max_body_size(Some(5 * 1024 * 1024))
        // Serve static files
        .serve_static("/static/", "static")
        // Home route
        .get("/", |_req: Request| async {
            Ok(Response::text("Visit /api/hello or /admin/dashboard"))
        })
        // API routes group
        .group("/api", |group| {
            group
                .get("/hello", |_req| async {
                    Ok(Response::json(serde_json::json!({
                        "message": "Hello from API group!"
                    })))
                })
                .post("/login", |req: Request| async move {
                    #[derive(Deserialize)]
                    struct LoginRequest {
                        username: String,
                    }

                    let login: LoginRequest = req.json()?;

                    // Set a session cookie
                    let session_cookie = Cookie::new("session", "abc123")
                        .path("/")
                        .max_age(3600)
                        .http_only()
                        .same_site(SameSite::Lax);

                    Ok(Response::json(serde_json::json!({
                        "message": format!("Logged in as {}", login.username)
                    }))
                    .cookie(session_cookie))
                })
                .get("/profile", |req: Request| async move {
                    // Check for session cookie
                    let cookies = req.cookies();
                    if let Some(session) = cookies.get("session") {
                        Ok(Response::json(serde_json::json!({
                            "session": session,
                            "message": "You are authenticated!"
                        })))
                    } else {
                        Err(Error::unauthorized("Not logged in"))
                    }
                })
        })
        // Admin routes group
        .group("/admin", |group| {
            group
                .get("/dashboard", |_req| async {
                    Ok(Response::text("Admin Dashboard"))
                })
                .get("/users", |_req| async {
                    Ok(Response::json(serde_json::json!({
                        "users": ["alice", "bob", "charlie"]
                    })))
                })
        })
        // Cookie demo
        .get("/set-cookie", |_req: Request| async {
            let cookie = Cookie::new("demo", "cookie-value")
                .path("/")
                .max_age(3600);

            Ok(Response::text("Cookie set!").cookie(cookie))
        })
        .get("/get-cookie", |req: Request| async move {
            let cookies = req.cookies();
            if let Some(demo) = cookies.get("demo") {
                Ok(Response::text(format!("Cookie value: {}", demo)))
            } else {
                Ok(Response::text("No cookie found"))
            }
        })
        .run("127.0.0.1:3000")
        .await
        .unwrap();
}
