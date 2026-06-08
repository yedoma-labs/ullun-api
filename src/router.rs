//! Router implementation using matchit

use crate::error::{Error, Result};
use crate::handler::DynHandler;
use crate::request::Params;

/// HTTP method routing
#[derive(Default)]
pub struct Router {
    get: matchit::Router<DynHandler>,
    post: matchit::Router<DynHandler>,
    put: matchit::Router<DynHandler>,
    delete: matchit::Router<DynHandler>,
    patch: matchit::Router<DynHandler>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, method: &str, path: &str, handler: DynHandler) -> Result<()> {
        let router = match method.to_uppercase().as_str() {
            "GET" => &mut self.get,
            "POST" => &mut self.post,
            "PUT" => &mut self.put,
            "DELETE" => &mut self.delete,
            "PATCH" => &mut self.patch,
            _ => return Err(Error::bad_request(format!("Unsupported method: {}", method))),
        };

        router
            .insert(path, handler)
            .map_err(|e| Error::internal(format!("Failed to insert route: {}", e)))?;

        Ok(())
    }

    pub fn match_route(&self, method: &str, path: &str) -> Result<(DynHandler, Params)> {
        let router = match method.to_uppercase().as_str() {
            "GET" => &self.get,
            "POST" => &self.post,
            "PUT" => &self.put,
            "DELETE" => &self.delete,
            "PATCH" => &self.patch,
            _ => return Err(Error::not_found("Route not found")),
        };

        let matched = router
            .at(path)
            .map_err(|_| Error::not_found("Route not found"))?;

        let mut params = Params::new();
        for (key, value) in matched.params.iter() {
            params.insert(key.to_string(), value.to_string());
        }

        Ok((matched.value.clone(), params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::Handler;
    use crate::request::Request;
    use crate::response::Response;
    use std::sync::Arc;

    struct TestHandler;

    #[async_trait::async_trait]
    impl Handler for TestHandler {
        async fn call(&self, _req: Request) -> Result<Response> {
            Ok(Response::text("test"))
        }
    }

    #[test]
    fn test_router_insert_and_match() {
        let mut router = Router::new();
        let handler = Arc::new(TestHandler) as DynHandler;

        router.insert("GET", "/hello/{name}", handler.clone()).unwrap();
        
        let (_, params) = router.match_route("GET", "/hello/world").unwrap();
        assert_eq!(params.get("name").unwrap(), "world");
    }

    #[test]
    fn test_router_not_found() {
        let router = Router::new();
        assert!(router.match_route("GET", "/not-found").is_err());
    }
}
