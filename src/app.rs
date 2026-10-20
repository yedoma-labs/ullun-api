//! Main application orchestrator

use crate::error::{Error, Result};
use crate::handler::{DynHandler, Handler, HandlerWithParams};
use crate::middleware::{Layered, Middleware, Next};
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

        // Group middlewares wrap only this group's routes (inside the global ones)
        let middlewares = group.middlewares;
        for (method, path, handler) in group.routes {
            let full_path = format!("{}{}", group.prefix, path);
            let handler: DynHandler = if middlewares.is_empty() {
                handler
            } else {
                Arc::new(Layered::new(middlewares.clone(), handler))
            };
            if let Err(e) = self.router.insert(&method, &full_path, handler) {
                panic!("Failed to register {} route {}: {}", method, full_path, e);
            }
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

    /// Accept WebSocket connections on `path` (feature `websocket`)
    ///
    /// The handler receives the original request (path params, query, headers,
    /// extensions set by middleware) and the upgraded socket, and runs on its
    /// own task until it returns.
    #[cfg(feature = "websocket")]
    pub fn ws<F, Fut>(self, path: &str, handler: F) -> Self
    where
        F: Fn(Request, crate::websocket::WebSocket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.ws_with(path, crate::websocket::WsOptions::default(), handler)
    }

    /// Like [`App::ws`] with explicit size limit and allowed origins
    #[cfg(feature = "websocket")]
    pub fn ws_with<F, Fut>(
        mut self,
        path: &str,
        options: crate::websocket::WsOptions,
        handler: F,
    ) -> Self
    where
        F: Fn(Request, crate::websocket::WebSocket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let route = crate::websocket::WsRoute::new(handler, options);
        if let Err(e) = self
            .router
            .insert("GET", path, Arc::new(route) as DynHandler)
        {
            panic!("Failed to register WebSocket route {}: {}", path, e);
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
        // matchit 0.9 catch-all syntax: {*param} matches the rest of the path
        let prefix = route_prefix.trim_end_matches('/');
        let handler: DynHandler = Arc::new(handler);
        let route = format!("{}/{{*filepath}}", prefix);
        if let Err(e) = self.router.insert("GET", &route, handler.clone()) {
            panic!("Failed to register static file route {}: {}", route, e);
        }
        // The catch-all needs at least one segment, so also serve the prefix
        // root (-> index.html). Skipped when the user already owns that route.
        let root = if prefix.is_empty() { "/" } else { prefix };
        let _ = self.router.insert("GET", root, handler.clone());
        if !prefix.is_empty() {
            let _ = self.router.insert("GET", &format!("{}/", prefix), handler);
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

    /// Serve on an already bound listener until the future is dropped.
    ///
    /// Useful for tests, benchmarks and embedding: bind to port `0`, read the
    /// assigned port from `listener.local_addr()`, then call `serve`.
    /// Unlike [`App::run`], this does not install a CTRL+C handler.
    pub async fn serve(self, listener: TcpListener) -> Result<()> {
        Self::serve_loop(
            listener,
            Arc::new(self.router),
            Arc::new(self.middlewares),
            self.max_body_size,
        )
        .await
    }

    async fn serve_loop(
        listener: TcpListener,
        router: Arc<Router>,
        middlewares: Arc<Vec<Arc<dyn Middleware>>>,
        max_body_size: Option<usize>,
    ) -> Result<()> {
        loop {
            let (stream, peer) = listener
                .accept()
                .await
                .map_err(|e| Error::internal(format!("Failed to accept: {}", e)))?;

            let io = TokioIo::new(stream);
            let router = router.clone();
            let middlewares = middlewares.clone();

            tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(|req| {
                            let router = router.clone();
                            let middlewares = middlewares.clone();
                            async move {
                                Ok::<_, Infallible>(
                                    handle_request(req, peer, router, middlewares, max_body_size)
                                        .await,
                                )
                            }
                        }),
                    )
                    .with_upgrades()
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
        self.routes.push((
            "GET".to_string(),
            path.to_string(),
            Arc::new(handler) as DynHandler,
        ));
        self
    }

    pub fn post<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push((
            "POST".to_string(),
            path.to_string(),
            Arc::new(handler) as DynHandler,
        ));
        self
    }

    pub fn put<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push((
            "PUT".to_string(),
            path.to_string(),
            Arc::new(handler) as DynHandler,
        ));
        self
    }

    pub fn delete<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push((
            "DELETE".to_string(),
            path.to_string(),
            Arc::new(handler) as DynHandler,
        ));
        self
    }

    pub fn patch<H>(mut self, path: &str, handler: H) -> Self
    where
        H: Handler,
    {
        self.routes.push((
            "PATCH".to_string(),
            path.to_string(),
            Arc::new(handler) as DynHandler,
        ));
        self
    }

    /// Add middleware that runs only for the routes of this group
    ///
    /// It runs after the application-wide middleware, in registration order,
    /// regardless of whether it is added before or after the routes.
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
    peer: SocketAddr,
    router: Arc<Router>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
    max_body_size: Option<usize>,
) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    // Convert hyper request to our Request type
    let (parts, body) = req.into_parts();

    // Enforce body size limit during streaming
    let body_bytes = if let Some(max_size) = max_body_size {
        // Check Content-Length header first (fast path)
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

        // Limit actual stream reading (prevents bypass via chunked encoding)
        let limited = http_body_util::Limited::new(body, max_size);
        match limited.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => {
                // Body exceeded limit during reading
                let error_response = Response::from_error(Error::custom(
                    413,
                    format!("Request body too large (max {} bytes)", max_size),
                ));
                return error_response.into_hyper();
            }
        }
    } else {
        match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => bytes::Bytes::new(),
        }
    };

    let mut request = Request::new(parts.method, parts.uri, parts.headers, body_bytes);
    request.extensions = parts.extensions;
    request.remote_addr = Some(peer);

    // Match route. A miss still runs the middleware chain (CORS preflight,
    // request ids, logging, security headers) and ends in a 404.
    let handler: Option<DynHandler> = match router.match_route(&method, &path) {
        Ok((handler, params)) => {
            request.params = params;
            Some(handler)
        }
        Err(_) => None,
    };

    let result = if middlewares.is_empty() {
        dispatch(handler, request).await
    } else {
        // Route handler (or the 404) runs as the last link of the chain
        let mut chain = middlewares.to_vec();
        let final_handler = Arc::new(move |req: Request, _next: Next| {
            let handler = handler.clone();
            async move { dispatch(handler, req).await }
        });
        chain.push(final_handler as Arc<dyn Middleware>);

        Next::new(Arc::new(chain)).run(request).await
    };

    // Convert Result to Response
    let response = match result {
        Ok(resp) => resp,
        Err(err) => Response::from_error(err),
    };

    response.into_hyper()
}

async fn dispatch(handler: Option<DynHandler>, req: Request) -> Result<Response> {
    match handler {
        Some(handler) => handler.call(req).await,
        None => Err(Error::not_found("Route not found")),
    }
}
