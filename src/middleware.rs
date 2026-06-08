//! Middleware types and traits

use crate::error::{Error, Result};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use std::sync::Arc;

/// Middleware trait
#[async_trait]
pub trait Middleware: Send + Sync + 'static {
    async fn handle(&self, req: Request, next: Next) -> Result<Response>;
}

/// Next middleware in chain
#[derive(Clone)]
pub struct Next {
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
    index: usize,
}

impl Next {
    pub fn new(middlewares: Arc<Vec<Arc<dyn Middleware>>>) -> Self {
        Self {
            middlewares,
            index: 0,
        }
    }

    pub async fn run(mut self, req: Request) -> Result<Response> {
        if self.index < self.middlewares.len() {
            let middleware = self.middlewares[self.index].clone();
            self.index += 1;
            middleware.handle(req, self).await
        } else {
            // End of chain without handler match
            Err(Error::not_found("Route not found"))
        }
    }
}

/// Implement Middleware for async functions
#[async_trait]
impl<F, Fut> Middleware for F
where
    F: Fn(Request, Next) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Response>> + Send + 'static,
{
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        (self)(req, next).await
    }
}

/// Logger middleware
pub async fn logger(req: Request, next: Next) -> Result<Response> {
    let method = req.method.clone();
    let uri = req.uri.clone();
    let start = std::time::Instant::now();

    let response = next.run(req).await;

    let duration = start.elapsed();
    println!("{} {} - {:?}", method, uri, duration);

    response
}

/// CORS middleware with preflight support
pub fn cors(
    allowed_origins: Vec<String>,
) -> impl Fn(
    Request,
    Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send>>
       + Clone {
    move |req: Request, next: Next| {
        let origins = allowed_origins.clone();
        Box::pin(async move {
            // Handle preflight OPTIONS requests
            if req.method == http::Method::OPTIONS {
                let mut response = Response::new(http::StatusCode::NO_CONTENT);

                if !origins.is_empty() {
                    let origin = if origins.contains(&"*".to_string()) {
                        "*".to_string()
                    } else {
                        // In production, should check Origin header against allowed list
                        origins[0].clone()
                    };
                    response = response.with_header("Access-Control-Allow-Origin", origin);
                    response = response.with_header(
                        "Access-Control-Allow-Methods",
                        "GET, POST, PUT, DELETE, PATCH, OPTIONS",
                    );
                    response = response.with_header(
                        "Access-Control-Allow-Headers",
                        "Content-Type, Authorization",
                    );
                    response = response.with_header("Access-Control-Max-Age", "86400");
                }

                return Ok(response);
            }

            // Regular request - add CORS headers to response
            let mut response = next.run(req).await?;

            if !origins.is_empty() {
                let origin = if origins.contains(&"*".to_string()) {
                    "*".to_string()
                } else {
                    origins[0].clone()
                };
                response = response.with_header("Access-Control-Allow-Origin", origin);
                response = response.with_header("Access-Control-Allow-Credentials", "true");
            }

            Ok(response)
        })
    }
}
