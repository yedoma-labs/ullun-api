//! Complete API example showcasing v0.2.0 features
//!
//! Run: cargo run --example complete_api
//! Test: curl http://localhost:3000

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use ullun::cookies::SameSite;
use ullun::prelude::*;

// In-memory database
#[derive(Clone)]
struct Database {
    items: Arc<RwLock<HashMap<u64, Item>>>,
    sessions: Arc<RwLock<HashMap<String, String>>>, // session_id -> username
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Item {
    id: u64,
    name: String,
    price: f64,
}

impl Database {
    fn new() -> Self {
        let mut items = HashMap::new();
        items.insert(
            1,
            Item {
                id: 1,
                name: "Widget".to_string(),
                price: 9.99,
            },
        );
        items.insert(
            2,
            Item {
                id: 2,
                name: "Gadget".to_string(),
                price: 19.99,
            },
        );

        Self {
            items: Arc::new(RwLock::new(items)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

// Request/Response types
#[derive(Deserialize)]
struct LoginRequest {
    username: String,
}

#[derive(Deserialize)]
struct CreateItemRequest {
    name: String,
    price: f64,
}

#[tokio::main]
async fn main() {
    let db = Database::new();

    // Create static files
    std::fs::create_dir_all("public").ok();
    std::fs::write("public/index.html", 
        "<h1>Complete API Example</h1><p>See <a href='/api/items'>items</a></p>").ok();

    println!("🚀 Server running on http://127.0.0.1:3000");
    println!("📁 Static: http://127.0.0.1:3000/");
    println!("📋 Items:  http://127.0.0.1:3000/api/items");
    println!();

    let db_clone = db.clone();

    App::new()
        // Configure body size limit
        .max_body_size(Some(1024 * 1024)) // 1MB
        
        // Serve static files
        .serve_static("/", "public")
        
        // Health check
        .get("/health", |_req| async {
            Ok(Response::json(serde_json::json!({"status": "ok"})))
        })
        
        // API routes group
        .group("/api", |group| {
            group
                // Authentication
                .post("/login", {
                    let db = db_clone.clone();
                    move |req: Request| {
                        let db = db.clone();
                        async move {
                            let login: LoginRequest = req.json()?;
                            
                            // Create session
                            let session_id = format!("sess_{}", chrono::Utc::now().timestamp());
                            db.sessions.write().await.insert(session_id.clone(), login.username.clone());
                            
                            // Set cookie
                            let cookie = Cookie::new("session", session_id)
                                .path("/")
                                .max_age(3600)
                                .http_only()
                                .same_site(SameSite::Lax);
                            
                            Ok(Response::json(serde_json::json!({
                                "message": "Logged in",
                                "username": login.username
                            })).cookie(cookie))
                        }
                    }
                })
                
                // Get current user
                .get("/me", {
                    let db = db_clone.clone();
                    move |req: Request| {
                        let db = db.clone();
                        async move {
                            let cookies = req.cookies();
                            let session_id = cookies.get("session")
                                .ok_or_else(|| Error::unauthorized("Not logged in"))?;
                            
                            let sessions = db.sessions.read().await;
                            let username = sessions.get(session_id)
                                .ok_or_else(|| Error::unauthorized("Invalid session"))?;
                            
                            Ok(Response::json(serde_json::json!({
                                "username": username
                            })))
                        }
                    }
                })
                
                // List items
                .get("/items", {
                    let db = db_clone.clone();
                    move |_req: Request| {
                        let db = db.clone();
                        async move {
                            let items = db.items.read().await;
                            let item_list: Vec<&Item> = items.values().collect();
                            Ok(Response::json(serde_json::json!({ "items": item_list })))
                        }
                    }
                })
        })
        
        // Item routes (with path params)
        .get_with_params("/api/items/{id}", {
            let db = db.clone();
            move |params: Params| {
                let db = db.clone();
                async move {
                    let id: u64 = params.get("id")?.parse()
                        .map_err(|_| Error::bad_request("Invalid ID"))?;
                    
                    let items = db.items.read().await;
                    let item = items.get(&id)
                        .ok_or_else(|| Error::not_found("Item not found"))?;
                    
                    Ok(Response::json(item))
                }
            }
        })
        
        .post("/api/items", {
            let db = db.clone();
            move |req: Request| {
                let db = db.clone();
                async move {
                    let create: CreateItemRequest = req.json()?;
                    
                    let mut items = db.items.write().await;
                    let id = items.len() as u64 + 1;
                    
                    let item = Item {
                        id,
                        name: create.name,
                        price: create.price,
                    };
                    
                    items.insert(id, item.clone());
                    
                    Ok(Response::json(serde_json::json!({
                        "message": "Created",
                        "item": item
                    })))
                }
            }
        })
        
        .run("127.0.0.1:3000")
        .await
        .unwrap();

    // Cleanup
    std::fs::remove_dir_all("public").ok();
}
