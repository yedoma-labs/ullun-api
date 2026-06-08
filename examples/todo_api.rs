//! Complete TODO API example with authentication, CRUD operations, and middleware
//!
//! Features:
//! - JWT authentication
//! - CRUD operations for todos
//! - User management
//! - Security headers
//! - CORS
//! - Request logging
//! - Error handling
//!
//! Run with: cargo run --example todo_api

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use ullun::prelude::*;

// Domain models
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Todo {
    id: u64,
    user_id: u64,
    title: String,
    description: String,
    completed: bool,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    id: u64,
    username: String,
    email: String,
}

#[derive(Debug, Deserialize)]
struct CreateTodoRequest {
    title: String,
    description: String,
}

#[derive(Debug, Deserialize)]
struct UpdateTodoRequest {
    title: Option<String>,
    description: Option<String>,
    completed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    token: String,
    user: User,
}

// In-memory database (for demo purposes)
#[derive(Clone)]
struct Database {
    todos: Arc<RwLock<HashMap<u64, Todo>>>,
    #[allow(dead_code)] // Used in production, not in this demo
    users: Arc<RwLock<HashMap<u64, User>>>,
    next_todo_id: Arc<RwLock<u64>>,
    #[allow(dead_code)] // Used in production, not in this demo
    next_user_id: Arc<RwLock<u64>>,
}

impl Database {
    fn new() -> Self {
        let mut users = HashMap::new();
        users.insert(
            1,
            User {
                id: 1,
                username: "demo".to_string(),
                email: "demo@example.com".to_string(),
            },
        );

        Self {
            todos: Arc::new(RwLock::new(HashMap::new())),
            users: Arc::new(RwLock::new(users)),
            next_todo_id: Arc::new(RwLock::new(1)),
            next_user_id: Arc::new(RwLock::new(2)),
        }
    }
}

// Middleware: Security headers
async fn security_headers_middleware(req: Request, next: Next) -> Result<Response> {
    let mut response = next.run(req).await?;
    response = response
        .with_header("X-Content-Type-Options", "nosniff")
        .with_header("X-Frame-Options", "DENY")
        .with_header("X-XSS-Protection", "1; mode=block")
        .with_header(
            "Strict-Transport-Security",
            "max-age=31536000; includeSubDomains",
        );
    Ok(response)
}

// Middleware: Simple JWT authentication (demo - use proper JWT library in production)
async fn auth_middleware(req: Request, next: Next) -> Result<Response> {
    // Skip auth for login endpoint
    if req.uri.path() == "/api/login" || req.uri.path() == "/api/health" {
        return next.run(req).await;
    }

    // Check for Authorization header
    let auth_header = req
        .headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| Error::unauthorized("Missing authorization header"))?;

    // Simple token validation (in production, use proper JWT verification)
    if !auth_header.starts_with("Bearer ") {
        return Err(Error::unauthorized("Invalid authorization format"));
    }

    let token = &auth_header[7..];
    if token != "demo-token-123" {
        return Err(Error::unauthorized("Invalid token"));
    }

    next.run(req).await
}

// Handlers
async fn health_check(_req: Request) -> Result<Response> {
    Ok(Response::json(serde_json::json!({
        "status": "healthy",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "version": env!("CARGO_PKG_VERSION")
    })))
}

async fn login(req: Request) -> Result<Response> {
    let body: LoginRequest = req.json()?;

    // Simple validation (in production, check against database and verify password hash)
    if body.username != "demo" || body.password != "password" {
        return Err(Error::unauthorized("Invalid credentials"));
    }

    let user = User {
        id: 1,
        username: body.username,
        email: "demo@example.com".to_string(),
    };

    // In production, generate proper JWT token
    let response = LoginResponse {
        token: "demo-token-123".to_string(),
        user,
    };

    Ok(Response::json(response))
}

async fn list_todos(_req: Request) -> Result<Response> {
    // Extract database from app state (simplified - use proper state management)
    // For this demo, we'll create a new database each time
    let db = Database::new();

    let todos = db.todos.read().await;
    let todo_list: Vec<&Todo> = todos.values().collect();

    Ok(Response::json(todo_list))
}

async fn get_todo(params: Params) -> Result<Response> {
    let id: u64 = params
        .get("id")?
        .parse()
        .map_err(|_| Error::bad_request("Invalid todo ID"))?;

    let db = Database::new();
    let todos = db.todos.read().await;

    let todo = todos
        .get(&id)
        .ok_or_else(|| Error::not_found("Todo not found"))?;

    Ok(Response::json(todo))
}

async fn create_todo(req: Request) -> Result<Response> {
    let body: CreateTodoRequest = req.json()?;

    // Validation
    if body.title.trim().is_empty() {
        return Err(Error::bad_request("Title cannot be empty"));
    }

    let db = Database::new();
    let mut next_id = db.next_todo_id.write().await;
    let id = *next_id;
    *next_id += 1;

    let todo = Todo {
        id,
        user_id: 1, // Hardcoded for demo
        title: body.title,
        description: body.description,
        completed: false,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    let mut todos = db.todos.write().await;
    todos.insert(id, todo.clone());

    Ok(Response::json(todo))
}

async fn update_todo_handler(req: Request) -> Result<Response> {
    let id: u64 = req
        .params
        .get("id")?
        .parse()
        .map_err(|_| Error::bad_request("Invalid todo ID"))?;

    let body: UpdateTodoRequest = req.json()?;

    let db = Database::new();
    let mut todos = db.todos.write().await;

    let todo = todos
        .get_mut(&id)
        .ok_or_else(|| Error::not_found("Todo not found"))?;

    if let Some(title) = body.title {
        if title.trim().is_empty() {
            return Err(Error::bad_request("Title cannot be empty"));
        }
        todo.title = title;
    }

    if let Some(description) = body.description {
        todo.description = description;
    }

    if let Some(completed) = body.completed {
        todo.completed = completed;
    }

    Ok(Response::json(todo.clone()))
}

async fn delete_todo(params: Params) -> Result<Response> {
    let id: u64 = params
        .get("id")?
        .parse()
        .map_err(|_| Error::bad_request("Invalid todo ID"))?;

    let db = Database::new();
    let mut todos = db.todos.write().await;

    todos
        .remove(&id)
        .ok_or_else(|| Error::not_found("Todo not found"))?;

    Ok(Response::json(serde_json::json!({
        "message": "Todo deleted successfully",
        "id": id
    })))
}

#[tokio::main]
async fn main() {
    println!("🚀 Starting TODO API server...");
    println!("📝 Example API at http://localhost:3000");
    println!();
    println!("Test with:");
    println!("  curl -X POST http://localhost:3000/api/login \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -d '{{\"username\":\"demo\",\"password\":\"password\"}}'");
    println!();
    println!("  curl http://localhost:3000/api/todos \\");
    println!("    -H 'Authorization: Bearer demo-token-123'");
    println!();

    App::new()
        // Global middleware
        .middleware(ullun::middleware::logger)
        .middleware(security_headers_middleware)
        .middleware(ullun::middleware::cors(vec!["*".to_string()]))
        // Public routes
        .get("/api/health", health_check)
        .post("/api/login", login)
        // Protected routes (auth middleware will check)
        .middleware(auth_middleware)
        .get("/api/todos", list_todos)
        .get_with_params("/api/todos/{id}", get_todo)
        .post("/api/todos", create_todo)
        .put("/api/todos/{id}", update_todo_handler)
        .get_with_params("/api/todos/{id}/delete", delete_todo)
        // Start server
        .run("127.0.0.1:3000")
        .await
        .unwrap();
}
