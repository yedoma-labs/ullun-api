//! Main application orchestrator

use crate::error::{Error, Result};
use crate::handler::{BoxFuture, DynHandler, Handler, HandlerWithParams};
use crate::middleware::{Layered, Middleware, Next};
use crate::openapi::{self, OpenApiInfo, Operation, RouteKind};
use crate::request::{Params, Request, StateMap};
use crate::response::Response;
use crate::router::Router;
use crate::shutdown::{Context, Shutdown};
use http_body_util::BodyExt;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// Main application struct
pub struct App {
    router: Router,
    middlewares: Vec<Arc<dyn Middleware>>,
    max_body_size: Option<usize>, // in bytes
    docs: HashMap<(String, String), Operation>,
    openapi: Option<OpenApiInfo>,
    state: StateMap,
    background: Vec<BackgroundTask>,
    shutdown_grace: Duration,
}

type BackgroundTask = Box<dyn FnOnce(Context) -> BoxFuture<'static, ()> + Send>;

/// Everything a running server shares between connections
struct Shared {
    router: Router,
    middlewares: Vec<Arc<dyn Middleware>>,
    max_body_size: Option<usize>,
    state: Arc<StateMap>,
    shutdown: Shutdown,
}

/// Route group builder
pub struct RouteGroup {
    prefix: String,
    routes: Vec<(String, String, DynHandler)>, // (method, path, handler)
    middlewares: Vec<Arc<dyn Middleware>>,
    docs: Vec<(String, String, Operation)>, // (method, path relative to prefix, docs)
}

impl App {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            middlewares: Vec::new(),
            max_body_size: Some(10 * 1024 * 1024), // 10 MB default
            docs: HashMap::new(),
            openapi: None,
            state: StateMap::default(),
            background: Vec::new(),
            shutdown_grace: Duration::from_secs(10),
        }
    }

    /// Set maximum request body size in bytes (None = unlimited)
    pub fn max_body_size(mut self, size: Option<usize>) -> Self {
        self.max_body_size = size;
        self
    }

    /// Register shared application state, readable in handlers and
    /// middleware with [`Request::state`]
    ///
    /// One value per type; registering the same type again replaces it. Use
    /// interior mutability (`Mutex`, `RwLock`, atomics) for mutable state: the
    /// value is shared by all requests and is not cloned.
    ///
    /// ```
    /// use ullun::prelude::*;
    /// use std::sync::atomic::{AtomicU64, Ordering};
    ///
    /// #[derive(Default)]
    /// struct Hits(AtomicU64);
    ///
    /// async fn count(req: Request) -> Result<Response> {
    ///     let hits = req.state::<Hits>()?.0.fetch_add(1, Ordering::Relaxed) + 1;
    ///     Ok(Response::text(hits.to_string()))
    /// }
    ///
    /// let app = App::new().state(Hits::default()).get("/", count);
    /// ```
    pub fn state<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.state.insert(value);
        self
    }

    /// Run `task` alongside the server, started when it starts serving
    ///
    /// The task gets a [`Context`] with the shared state and the shutdown
    /// signal; it should return when `ctx.sleep(..)` reports `false` or
    /// `ctx.shutdown().wait()` completes. Finished tasks are awaited during
    /// shutdown, for at most [`App::shutdown_grace`].
    ///
    /// ```
    /// use std::time::Duration;
    /// use ullun::prelude::*;
    ///
    /// struct Jobs;
    ///
    /// let app = App::new().state(Jobs).background(|ctx: Context| async move {
    ///     while ctx.sleep(Duration::from_secs(60)).await {
    ///         let _jobs = ctx.state::<Jobs>().unwrap();
    ///         // purge expired sessions, refresh caches, ...
    ///     }
    /// });
    /// ```
    pub fn background<F, Fut>(mut self, task: F) -> Self
    where
        F: FnOnce(Context) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.background
            .push(Box::new(move |ctx| Box::pin(task(ctx))));
        self
    }

    /// How long shutdown waits for in-flight requests and background tasks
    /// (default 10 seconds)
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// Create a route group with a common prefix
    pub fn group(mut self, prefix: &str, configure: impl FnOnce(RouteGroup) -> RouteGroup) -> Self {
        let group = RouteGroup::new(prefix);
        let group = configure(group);

        for (method, path, op) in group.docs {
            let full_path = format!("{}{}", group.prefix, path);
            self.docs.insert((method, full_path), op);
        }

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

    /// Describe a registered route for the OpenAPI document
    ///
    /// `path` is the path exactly as registered, e.g. `/users/{id}`.
    pub fn doc(mut self, method: &str, path: &str, operation: Operation) -> Self {
        self.docs
            .insert((method.to_ascii_uppercase(), path.to_string()), operation);
        self
    }

    /// Publish an OpenAPI 3.1 document of all routes (and optionally Swagger UI)
    ///
    /// See the [`openapi`] module.
    pub fn openapi(mut self, info: OpenApiInfo) -> Self {
        self.openapi = Some(info);
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
        if let Err(e) = self.router.insert_as(
            RouteKind::WebSocket,
            "GET",
            path,
            Arc::new(route) as DynHandler,
        ) {
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
        if let Err(e) = self
            .router
            .insert_as(RouteKind::Hidden, "GET", &route, handler.clone())
        {
            panic!("Failed to register static file route {}: {}", route, e);
        }
        // The catch-all needs at least one segment, so also serve the prefix
        // root (-> index.html). Skipped when the user already owns that route.
        let root = if prefix.is_empty() { "/" } else { prefix };
        let _ = self
            .router
            .insert_as(RouteKind::Hidden, "GET", root, handler.clone());
        if !prefix.is_empty() {
            let _ =
                self.router
                    .insert_as(RouteKind::Hidden, "GET", &format!("{}/", prefix), handler);
        }
        self
    }

    /// Run the server until CTRL+C or SIGTERM, then shut down gracefully
    ///
    /// Stops accepting connections, lets in-flight requests finish and waits
    /// for background tasks, each for at most [`App::shutdown_grace`].
    pub async fn run(self, addr: &str) -> Result<()> {
        let addr: SocketAddr = addr
            .parse()
            .map_err(|e| Error::internal(format!("Invalid address: {}", e)))?;

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| Error::internal(format!("Failed to bind: {}", e)))?;

        println!("🚀 ullun-api listening on http://{}", addr);

        let result = self
            .serve_with_shutdown(listener, async {
                termination_signal().await;
                println!("\n🛑 Shutting down gracefully...");
            })
            .await;
        println!("✅ Server stopped");
        result
    }

    /// Serve on an already bound listener until the future is dropped.
    ///
    /// Useful for tests, benchmarks and embedding: bind to port `0`, read the
    /// assigned port from `listener.local_addr()`, then call `serve`.
    /// Unlike [`App::run`], this does not install signal handlers; use
    /// [`App::serve_with_shutdown`] for a graceful stop.
    pub async fn serve(self, listener: TcpListener) -> Result<()> {
        self.serve_with_shutdown(listener, std::future::pending())
            .await
    }

    /// Serve until `signal` completes, then shut down gracefully
    ///
    /// New connections are refused, idle keep-alive connections are closed,
    /// running requests finish, the shutdown signal reaches
    /// [`Request::shutdown_signal`] and background tasks, and the call returns
    /// once everything has stopped or [`App::shutdown_grace`] has passed.
    pub async fn serve_with_shutdown(
        mut self,
        listener: TcpListener,
        signal: impl std::future::Future<Output = ()>,
    ) -> Result<()> {
        self = self.finalize();
        let grace = self.shutdown_grace;
        let background = std::mem::take(&mut self.background);
        let (trigger, rx) = tokio::sync::watch::channel(false);
        let shutdown = Shutdown::new(rx);
        let shared = Arc::new(self.into_shared(shutdown.clone()));

        let tasks: Vec<_> = background
            .into_iter()
            .map(|task| {
                let ctx = Context::new(shared.state.clone(), shutdown.clone());
                tokio::spawn(task(ctx))
            })
            .collect();

        // every connection holds a clone; the channel closes when all are done
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::pin!(signal);
        loop {
            tokio::select! {
                _ = &mut signal => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        tokio::spawn(serve_connection(
                            stream,
                            peer,
                            shared.clone(),
                            shutdown.clone(),
                            done_tx.clone(),
                        ));
                    }
                    Err(e) => {
                        // typically out of file descriptors: back off instead of dying
                        eprintln!("Failed to accept connection: {}", e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }

        let _ = trigger.send(true);
        drop(done_tx);
        drop(listener);
        let drain = async {
            let _ = done_rx.recv().await;
            for task in tasks {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(grace, drain).await.is_err() {
            eprintln!(
                "Shutdown grace period of {:?} elapsed, stopping anyway",
                grace
            );
        }
        Ok(())
    }

    fn into_shared(self, shutdown: Shutdown) -> Shared {
        Shared {
            router: self.router,
            middlewares: self.middlewares,
            max_body_size: self.max_body_size,
            state: Arc::new(self.state),
            shutdown,
        }
    }

    /// Last step before serving: publish the OpenAPI document, now that all
    /// routes are known.
    fn finalize(mut self) -> Self {
        let Some(info) = self.openapi.take() else {
            return self;
        };
        let spec = openapi::build(&info, self.router.registered(), &self.docs);
        let spec = bytes::Bytes::from(serde_json::to_vec(&spec).unwrap_or_default());

        let spec_handler = {
            let spec = spec.clone();
            move |_req: Request| {
                let spec = spec.clone();
                async move {
                    Ok(Response::ok()
                        .with_header("content-type", "application/json")
                        .with_body(spec))
                }
            }
        };
        self.insert_hidden("GET", &info.spec_path, Arc::new(spec_handler));

        if let Some(docs_path) = &info.docs_path {
            let page = openapi::swagger_ui_html(&info.title, &info.spec_path);
            let page_handler = move |_req: Request| {
                let page = page.clone();
                async move { Ok(Response::html(page)) }
            };
            self.insert_hidden("GET", docs_path, Arc::new(page_handler));
        }
        self
    }

    fn insert_hidden(&mut self, method: &str, path: &str, handler: DynHandler) {
        if let Err(e) = self
            .router
            .insert_as(RouteKind::Hidden, method, path, handler)
        {
            panic!("Failed to register {} route {}: {}", method, path, e);
        }
    }
}

/// One connection, until it closes or shutdown drains it
async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
    shutdown: Shutdown,
    _done: tokio::sync::mpsc::Sender<()>,
) {
    let io = TokioIo::new(stream);
    let service = service_fn(|req| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(handle_request(req, peer, shared).await) }
    });
    let conn = http1::Builder::new()
        .serve_connection(io, service)
        .with_upgrades();
    tokio::pin!(conn);

    let mut draining = false;
    loop {
        tokio::select! {
            result = conn.as_mut() => {
                if let Err(err) = result {
                    eprintln!("Error serving connection: {:?}", err);
                }
                return;
            }
            _ = shutdown.wait(), if !draining => {
                // idle connections close now, busy ones after their response
                conn.as_mut().graceful_shutdown();
                draining = true;
            }
        }
    }
}

/// CTRL+C everywhere, plus SIGTERM on Unix (what container runtimes send)
async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => eprintln!("Cannot listen for SIGTERM: {}", e),
        }
    }
    let _ = tokio::signal::ctrl_c().await;
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
            docs: Vec::new(),
        }
    }

    /// Describe a route of this group for the OpenAPI document; `path` is
    /// relative to the group prefix, as given to `get`/`post`/...
    pub fn doc(mut self, method: &str, path: &str, operation: Operation) -> Self {
        self.docs
            .push((method.to_ascii_uppercase(), path.to_string(), operation));
        self
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
    shared: Arc<Shared>,
) -> hyper::Response<crate::response::HyperBody> {
    let Shared {
        router,
        middlewares,
        max_body_size,
        state,
        shutdown,
    } = &*shared;
    let max_body_size = *max_body_size;
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
    request.state = Some(state.clone());
    request.shutdown = Some(shutdown.clone());

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
