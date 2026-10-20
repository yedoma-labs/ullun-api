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

/// Share one middleware instance between several registrations, e.g. a
/// `RateLimiter` whose budget spans multiple groups
#[async_trait]
impl<M: Middleware + ?Sized> Middleware for Arc<M> {
    async fn handle(&self, req: Request, next: Next) -> Result<Response> {
        (**self).handle(req, next).await
    }
}

/// A handler wrapped in its own middleware stack (used by route groups)
pub(crate) struct Layered {
    middlewares: Vec<Arc<dyn Middleware>>,
    inner: crate::handler::DynHandler,
}

impl Layered {
    pub(crate) fn new(
        middlewares: Vec<Arc<dyn Middleware>>,
        inner: crate::handler::DynHandler,
    ) -> Self {
        Self { middlewares, inner }
    }
}

#[async_trait]
impl crate::handler::Handler for Layered {
    async fn call(&self, req: Request) -> Result<Response> {
        let inner = self.inner.clone();
        let mut chain = self.middlewares.clone();
        chain.push(Arc::new(move |req: Request, _next: Next| {
            let inner = inner.clone();
            async move { inner.call(req).await }
        }));
        Next::new(Arc::new(chain)).run(req).await
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
///
/// Kept for compatibility; delegates to [`crate::cors::Cors`], which also fixes
/// the old behaviour of answering every origin with the first configured one.
#[deprecated(since = "0.4.4", note = "use ullun::cors::Cors")]
pub fn cors(
    allowed_origins: Vec<String>,
) -> impl Fn(
    Request,
    Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send>>
       + Clone {
    let cors = if allowed_origins.iter().any(|o| o == "*") {
        crate::cors::Cors::new().allow_any_origin()
    } else {
        crate::cors::Cors::new()
            .allow_origins(allowed_origins)
            .allow_credentials()
    };
    let cors = Arc::new(cors);
    move |req: Request, next: Next| {
        let cors = cors.clone();
        Box::pin(async move { cors.handle(req, next).await })
    }
}
