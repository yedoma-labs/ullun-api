//! Main application orchestrator

use crate::error::{Error, Result};
use crate::handler::{DynHandler, Handler, HandlerWithParams};
use crate::middleware::{Middleware, Next};
use crate::request::{Params, Request};
use crate::response::Response;
use crate::router::Router;
use http_body_util::BodyExt;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Main application struct
pub struct App {
    router: Router,
    middlewares: Vec<Arc<dyn Middleware>>,
    max_body_size: Option<usize>, // in bytes
}

/// Route group builder
pub struct RouteGroup {
    prefix: String,
    routes: Vec<(String, String, DynHandler)>, // (method, path, handler)
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl App {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            middlewares: Vec::new(),
            max_body_size: Some(10 * 1024 * 1024), // 10 MB default
        }
    }

    /// Set maximum request body size in bytes (None = unlimited)
    pub fn max_body_size(mut self, size: Option<usize>) -> Self {
        self.max_body_size = size;
        self
    }

    /// Create a route group with a common prefix
    pub fn group(mut self, prefix: &str, configure: impl FnOnce(RouteGroup) -> RouteGroup) -> Self {
        let group = RouteGroup::new(prefix);
        let group = configure(group);
        
        // Register all routes with prefix
        for (method, path, handler) in group.routes {
            let full_path = format!("{}{}", group.prefix, path);
            if let Err(e) = self.router.insert(&method, &full_path, handler) {
                panic!("Failed to register {} route {}: {}", method, full_path, e);
            }
        }
        
        // Add group middlewares
        for middleware in group.middlewares {
            self.middlewares.push(middleware);
        }
        
        self
    }

    /// Add GET route
    pub fn get<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        if let Err(e) = self
            .router
            .insert("GET", path, Arc::new(handler) as DynHandler)
        {
            panic!("Failed to register GET route {}: {}", path, e);
        }
        self
    }

    /// Add GET route with path params extractor
    pub fn get_with_params<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Params) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Response>> + Send + 'static,
    {
        if let Err(e) = self.router.insert(
            "GET",
            path,
            Arc::new(HandlerWithParams::new(handler)) as DynHandler,
        ) {
            panic!("Failed to register GET route {}: {}", path, e);
        }
        self
    }

    /// Add POST route
    pub fn post<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        if let Err(e) = self
            .router
            .insert("POST", path, Arc::new(handler) as DynHandler)
        {
            panic!("Failed to register POST route {}: {}", path, e);
        }
        self
    }

    /// Add PUT route
    pub fn put<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        if let Err(e) = self
            .router
            .insert("PUT", path, Arc::new(handler) as DynHandler)
        {
            panic!("Failed to register PUT route {}: {}", path, e);
        }
        self
    }

    /// Add DELETE route
    pub fn delete<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        if let Err(e) = self
            .router
            .insert("DELETE", path, Arc::new(handler) as DynHandler)
        {
            panic!("Failed to register DELETE route {}: {}", path, e);
        }
        self
    }

    /// Add PATCH route
    pub fn patch<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        if let Err(e) = self
            .router
            .insert("PATCH", path, Arc::new(handler) as DynHandler)
        {
            panic!("Failed to register PATCH route {}: {}", path, e);
        }
        self
    }

    /// Add middleware
    pub fn middleware<M>(mut self, middleware: M) -> Self
    where
        M: Middleware,
    {
        self.middlewares.push(Arc::new(middleware));
        self
    }

    /// Serve static files from a directory
    pub fn serve_static(mut self, route_prefix: &str, dir_path: &str) -> Self {
        use crate::static_files::StaticFileHandler;
        let handler = StaticFileHandler::new(dir_path);
        let route = format!("{}{{filepath:.*}}", route_prefix);
        if let Err(e) = self.router.insert("GET", &route, Arc::new(handler) as DynHandler) {
            panic!("Failed to register static file route {}: {}", route, e);
        }
        self
    }

    /// Run the server with graceful shutdown on CTRL+C
    pub async fn run(self, addr: &str) -> Result<()> {
        let addr: SocketAddr = addr
            .parse()
            .map_err(|e| Error::internal(format!("Invalid address: {}", e)))?;

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| Error::internal(format!("Failed to bind: {}", e)))?;

        println!("🚀 ullun-api listening on http://{}", addr);

        let router = Arc::new(self.router);
        let middlewares = Arc::new(self.middlewares);
        let max_body_size = self.max_body_size;

        // Graceful shutdown signal
        let shutdown = async {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to install CTRL+C handler");
            println!("\n🛑 Shutting down gracefully...");
        };

        tokio::select! {
            _ = shutdown => {
                println!("✅ Server stopped");
                Ok(())
            }
            result = Self::serve_loop(listener, router, middlewares, max_body_size) => result
        }
    }

    async fn serve_loop(
        listener: TcpListener,
        router: Arc<Router>,
        middlewares: Arc<Vec<Arc<dyn Middleware>>>,
        max_body_size: Option<usize>,
    ) -> Result<()> {
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| Error::internal(format!("Failed to accept: {}", e)))?;

            let io = TokioIo::new(stream);
            let router = router.clone();
            let middlewares = middlewares.clone();
            let max_body_size = max_body_size;

            tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(|req| {
                            let router = router.clone();
                            let middlewares = middlewares.clone();
                            let max_body_size = max_body_size;
                            async move {
                                Ok::<_, Infallible>(
                                    handle_request(req, router, middlewares, max_body_size).await,
                                )
                            }
                        }),
                    )
                    .await
                {
                    eprintln!("Error serving connection: {:?}", err);
                }
            });
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteGroup {
    fn new(prefix: &str) -> Self {
        let prefix = prefix.trim_end_matches('/');
        Self {
            prefix: prefix.to_string(),
            routes: Vec::new(),
            middlewares: Vec::new(),
        }
    }

    pub fn get<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push(("GET".to_string(), path.to_string(), Arc::new(handler) as DynHandler));
        self
    }

    pub fn post<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push(("POST".to_string(), path.to_string(), Arc::new(handler) as DynHandler));
        self
    }

    pub fn put<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push(("PUT".to_string(), path.to_string(), Arc::new(handler) as DynHandler));
        self
    }

    pub fn delete<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push(("DELETE".to_string(), path.to_string(), Arc::new(handler) as DynHandler));
        self
    }

    pub fn patch<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push(("PATCH".to_string(), path.to_string(), Arc::new(handler) as DynHandler));
        self
    }

    pub fn middleware<M>(mut self, middleware: M) -> Self
    where
        M: Middleware,
    {
        self.middlewares.push(Arc::new(middleware));
        self
    }
}

async fn handle_request(
    req: hyper::Request<hyper::body::Incoming>,
    router: Arc<Router>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
    max_body_size: Option<usize>,
) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    // Convert hyper request to our Request type
    let (parts, body) = req.into_parts();

    // Check Content-Length against max_body_size
    if let Some(max_size) = max_body_size {
        if let Some(content_length) = parts.headers.get(http::header::CONTENT_LENGTH) {
            if let Ok(length_str) = content_length.to_str() {
                if let Ok(length) = length_str.parse::<usize>() {
                    if length > max_size {
                        let error_response = Response::from_error(Error::custom(
                            413,
                            format!("Request body too large (max {} bytes)", max_size),
                        ));
                        return error_response.into_hyper();
                    }
                }
            }
        }
    }

    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => bytes::Bytes::new(),
    };

    let mut request = Request::new(parts.method, parts.uri, parts.headers, body_bytes);

    // Match route
    let result = match router.match_route(&method, &path) {
        Ok((handler, params)) => {
            request.params = params;

            // Run middlewares + handler
            if middlewares.is_empty() {
                handler.call(request).await
            } else {
                // Create middleware chain
                let mut chain = middlewares.to_vec();

                // Add final handler as last middleware
                let final_handler = Arc::new(move |req: Request, _next: Next| {
                    let h = handler.clone();
                    async move { h.call(req).await }
                });
                chain.push(final_handler as Arc<dyn Middleware>);

                let next = Next::new(Arc::new(chain));
                next.run(request).await
            }
        }
        Err(e) => Err(e),
    };

    // Convert Result to Response
    let response = match result {
        Ok(resp) => resp,
        Err(err) => Response::from_error(err),
    };

    response.into_hyper()
}
