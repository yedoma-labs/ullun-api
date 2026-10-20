//! Router implementation using matchit

use crate::error::{Error, Result};
use crate::handler::DynHandler;
use crate::openapi::{RouteInfo, RouteKind};
use crate::request::Params;

/// A registered handler together with the pattern it was registered under
#[derive(Clone)]
struct Entry {
    handler: DynHandler,
    pattern: std::sync::Arc<str>,
}

/// HTTP method routing
#[derive(Default)]
pub struct Router {
    get: matchit::Router<Entry>,
    post: matchit::Router<Entry>,
    put: matchit::Router<Entry>,
    delete: matchit::Router<Entry>,
    patch: matchit::Router<Entry>,
    registered: Vec<RouteInfo>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, method: &str, path: &str, handler: DynHandler) -> Result<()> {
        self.insert_as(RouteKind::Http, method, path, handler)
    }

    pub(crate) fn insert_as(
        &mut self,
        kind: RouteKind,
        method: &str,
        path: &str,
        handler: DynHandler,
    ) -> Result<()> {
        let method = method.to_uppercase();
        let router = match method.as_str() {
            "GET" => &mut self.get,
            "POST" => &mut self.post,
            "PUT" => &mut self.put,
            "DELETE" => &mut self.delete,
            "PATCH" => &mut self.patch,
            _ => {
                return Err(Error::bad_request(format!(
                    "Unsupported method: {}",
                    method
                )))
            }
        };

        router
            .insert(
                path,
                Entry {
                    handler,
                    pattern: path.into(),
                },
            )
            .map_err(|e| Error::internal(format!("Failed to insert route: {}", e)))?;

        self.registered.push(RouteInfo {
            method,
            path: path.to_string(),
            kind,
        });
        Ok(())
    }

    /// Methods with a route for `path`, in a fixed order, including the
    /// implicit `HEAD` (next to `GET`) and `OPTIONS`. Empty when the path is unknown.
    pub fn allowed_methods(&self, path: &str) -> Vec<&'static str> {
        let tables: [(&'static str, &matchit::Router<Entry>); 5] = [
            ("GET", &self.get),
            ("POST", &self.post),
            ("PUT", &self.put),
            ("DELETE", &self.delete),
            ("PATCH", &self.patch),
        ];
        let mut allowed = Vec::new();
        for (name, table) in tables {
            if table.at(path).is_ok() {
                allowed.push(name);
                if name == "GET" {
                    allowed.push("HEAD");
                }
            }
        }
        if !allowed.is_empty() {
            allowed.push("OPTIONS");
        }
        allowed
    }

    pub(crate) fn registered(&self) -> &[RouteInfo] {
        &self.registered
    }

    pub fn match_route(&self, method: &str, path: &str) -> Result<(DynHandler, Params)> {
        self.match_route_with_pattern(method, path)
            .map(|(handler, params, _)| (handler, params))
    }

    /// Like [`Router::match_route`], also returning the pattern the route was
    /// registered under (`/users/{id}`), a low-cardinality name for the route
    pub fn match_route_with_pattern(
        &self,
        method: &str,
        path: &str,
    ) -> Result<(DynHandler, Params, std::sync::Arc<str>)> {
        let router = match method.to_uppercase().as_str() {
            // HEAD is answered by the GET handler, the body is dropped afterwards
            "GET" | "HEAD" => &self.get,
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

        Ok((
            matched.value.handler.clone(),
            params,
            matched.value.pattern.clone(),
        ))
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

        router
            .insert("GET", "/hello/{name}", handler.clone())
            .unwrap();

        let (_, params) = router.match_route("GET", "/hello/world").unwrap();
        assert_eq!(params.get("name").unwrap(), "world");
    }

    #[test]
    fn allowed_methods_lists_what_the_path_supports() {
        let mut router = Router::new();
        let handler = Arc::new(TestHandler) as DynHandler;
        router
            .insert("GET", "/users/{id}", handler.clone())
            .unwrap();
        router
            .insert("DELETE", "/users/{id}", handler.clone())
            .unwrap();
        router.insert("POST", "/users", handler.clone()).unwrap();

        assert_eq!(
            router.allowed_methods("/users/7"),
            ["GET", "HEAD", "DELETE", "OPTIONS"]
        );
        assert_eq!(router.allowed_methods("/users"), ["POST", "OPTIONS"]);
        assert!(router.allowed_methods("/nothing").is_empty());
        // HEAD resolves like GET
        assert!(router.match_route("HEAD", "/users/7").is_ok());
        assert!(router.match_route("HEAD", "/users").is_err());
    }

    #[test]
    fn test_router_not_found() {
        let router = Router::new();
        assert!(router.match_route("GET", "/not-found").is_err());
    }
}
