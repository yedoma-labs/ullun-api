//! Full-featured API example with middleware, JSON, and error handling

use ullun::prelude::*;

#[derive(Serialize, Deserialize)]
struct User {
    id: u64,
    name: String,
    email: String,
}

#[derive(Serialize, Deserialize)]
struct CreateUserRequest {
    name: String,
    email: String,
}

#[tokio::main]
async fn main() {
    App::new()
        // Middleware
        .middleware(ullun::middleware::logger)
        .middleware(ullun::middleware::cors(vec!["*".to_string()]))
        // Routes
        .get("/", |_req: Request| async { Ok(Response::text("API v1")) })
        .get("/users/{id}", get_user)
        .post("/users", create_user)
        .put("/users/{id}", update_user)
        .delete("/users/{id}", delete_user)
        .run("127.0.0.1:3000")
        .await
        .unwrap();
}

async fn get_user(req: Request) -> Result<Response> {
    let id: u64 = req
        .params
        .get("id")?
        .parse()
        .map_err(|_| Error::bad_request("Invalid user ID"))?;

    let user = User {
        id,
        name: "John Doe".to_string(),
        email: "john@example.com".to_string(),
    };

    Ok(Json(user).into())
}

async fn create_user(req: Request) -> Result<Response> {
    let body: CreateUserRequest = req.json()?;

    let user = User {
        id: 1,
        name: body.name,
        email: body.email,
    };

    Ok(Json(user).into())
}

async fn update_user(req: Request) -> Result<Response> {
    let id: u64 = req
        .params
        .get("id")?
        .parse()
        .map_err(|_| Error::bad_request("Invalid user ID"))?;

    let body: CreateUserRequest = req.json()?;

    let user = User {
        id,
        name: body.name,
        email: body.email,
    };

    Ok(Json(user).into())
}

async fn delete_user(req: Request) -> Result<Response> {
    let id: u64 = req
        .params
        .get("id")?
        .parse()
        .map_err(|_| Error::bad_request("Invalid user ID"))?;

    Ok(Json(serde_json::json!({
        "message": format!("User {} deleted", id)
    }))
    .into())
}
