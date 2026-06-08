//! Handler traits and types

use crate::error::Result;
use crate::request::{Params, Request};
use crate::response::Response;
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed future for handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Handler trait for route handlers
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn call(&self, req: Request) -> Result<Response>;
}

/// Implement Handler for async functions
#[async_trait]
impl<F, Fut> Handler for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
{
    async fn call(&self, req: Request) -> Result<Response> {
        (self)(req).await
    }
}

/// Handler wrapper with params
pub struct HandlerWithParams<F> {
    handler: F,
}

impl<F> HandlerWithParams<F> {
    pub fn new(handler: F) -> Self {
        Self { handler }
    }
}

#[async_trait]
impl<F, Fut> Handler for HandlerWithParams<F>
where
    F: Fn(Params) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
{
    async fn call(&self, req: Request) -> Result<Response> {
        (self.handler)(req.params).await
    }
}

/// Type-erased handler
pub type DynHandler = Arc<dyn Handler>;
