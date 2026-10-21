//! Main application orchestrator

use crate::error::{Error, Result};
use crate::handler::{BoxFuture, DynHandler, Handler, HandlerWithParams};
use crate::limits::{self, ConnGuard, ConnLimits};
use crate::middleware::{Layered, Middleware, Next};
use crate::openapi::{self, OpenApiInfo, Operation, RouteKind};
use crate::request::{Params, Request, StateMap};
use crate::response::Response;
use crate::router::Router;
use crate::shutdown::{Context, Shutdown};
use http_body_util::BodyExt;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
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
    shutdown_delay: Duration,
    body_limits: Vec<(String, Option<usize>)>,
    header_read_timeout: Option<Duration>,
    max_connections: Option<usize>,
    max_connections_per_ip: Option<usize>,
    #[cfg(feature = "tls")]
    tls: Option<crate::tls::TlsConfig>,
    #[cfg(feature = "tls")]
    tls_reload: Option<Duration>,
}

/// Facts about one connection that every request on it inherits
struct ConnInfo {
    peer: SocketAddr,
    secure: bool,
    client_cert: Option<Vec<u8>>,
}

type BackgroundTask = Box<dyn FnOnce(Context) -> BoxFuture<'static, ()> + Send>;

/// Everything a running server shares between connections
struct Shared {
    router: Router,
    middlewares: Vec<Arc<dyn Middleware>>,
    max_body_size: Option<usize>,
    body_limits: Vec<(String, Option<usize>)>,
    state: Arc<StateMap>,
    shutdown: Shutdown,
    header_read_timeout: Option<Duration>,
}

/// Route group builder
pub struct RouteGroup {
    prefix: String,
    routes: Vec<(String, String, DynHandler)>, // (method, path, handler)
    middlewares: Vec<Arc<dyn Middleware>>,
    docs: Vec<(String, String, Operation)>, // (method, path relative to prefix, docs)
    body_limit: Option<Option<usize>>,
    /// Paths registered with `ws_with`, documented as WebSocket upgrades
    ws_paths: Vec<String>,
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
            shutdown_delay: Duration::ZERO,
            body_limits: Vec::new(),
            header_read_timeout: Some(Duration::from_secs(30)),
            max_connections: None,
            max_connections_per_ip: None,
            #[cfg(feature = "tls")]
            tls: None,
            #[cfg(feature = "tls")]
            tls_reload: None,
        }
    }

    /// Set maximum request body size in bytes (None = unlimited)
    pub fn max_body_size(mut self, size: Option<usize>) -> Self {
        self.max_body_size = size;
        self
    }

    /// Different request body limit for everything under `prefix`
    ///
    /// `None` removes the limit. The longest matching prefix wins over the
    /// default from [`App::max_body_size`], e.g. a small default with a large
    /// limit for an upload route:
    /// `App::new().max_body_size(Some(64 * 1024)).body_limit("/api/files", Some(25 << 20))`.
    pub fn body_limit(mut self, prefix: &str, limit: Option<usize>) -> Self {
        self.body_limits
            .push((prefix.trim_end_matches('/').to_string(), limit));
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

    /// Serve HTTPS instead of HTTP (feature `tls`), see [`crate::tls`]
    #[cfg(feature = "tls")]
    pub fn tls(mut self, config: crate::tls::TlsConfig) -> Self {
        self.tls = Some(config);
        self
    }

    /// Check the certificate files for changes this often and switch to renewed
    /// certificates without a restart (feature `tls`; needs
    /// `TlsConfig::from_pem_files`). A failed reload is logged and the previous
    /// certificate stays in use.
    #[cfg(feature = "tls")]
    pub fn tls_reload_every(mut self, interval: Duration) -> Self {
        self.tls_reload = Some(interval);
        self
    }

    /// How long a client may take to send the request headers (default 30
    /// seconds, `None` disables). Also applies to an idle keep-alive connection
    /// waiting for its next request, and stops slowloris-style clients that
    /// trickle bytes to hold connections open.
    pub fn header_read_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.header_read_timeout = timeout.into();
        self
    }

    /// Cap on simultaneously open connections. Further connections get an
    /// immediate `503` with `Retry-After` and are closed (plain `503` bytes
    /// even on a TLS port, which a TLS client reports as a failed handshake).
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = Some(max.max(1));
        self
    }

    /// Cap on simultaneously open connections from one peer IP, so a single
    /// client cannot use up [`App::max_connections`]. Behind a reverse proxy
    /// every connection comes from the proxy: leave this unset there.
    pub fn max_connections_per_ip(mut self, max: usize) -> Self {
        self.max_connections_per_ip = Some(max.max(1));
        self
    }

    /// How long shutdown waits for in-flight requests and background tasks
    /// (default 10 seconds)
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// Keep serving normally for this long after the stop signal before
    /// refusing connections (default none). Readiness checks
    /// ([`Health::readiness`](crate::health::Health::readiness)) answer `503`
    /// during this time, so a load balancer or Kubernetes stops routing to the
    /// instance first and no request is cut off; set it a little above the
    /// balancer's probe interval.
    pub fn shutdown_delay(mut self, delay: Duration) -> Self {
        self.shutdown_delay = delay;
        self
    }

    /// Create a route group with a common prefix
    pub fn group(mut self, prefix: &str, configure: impl FnOnce(RouteGroup) -> RouteGroup) -> Self {
        let group = RouteGroup::new(prefix);
        let group = configure(group);

        if let Some(limit) = group.body_limit {
            self.body_limits.push((group.prefix.clone(), limit));
        }
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
            let kind = if method == "GET" && group.ws_paths.contains(&path) {
                RouteKind::WebSocket
            } else {
                RouteKind::Http
            };
            if let Err(e) = self.router.insert_as(kind, &method, &full_path, handler) {
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

        #[cfg(feature = "tls")]
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        #[cfg(not(feature = "tls"))]
        let scheme = "http";
        println!("🚀 ullun-api listening on {}://{}", scheme, addr);

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
        let delay = self.shutdown_delay;
        let background = std::mem::take(&mut self.background);
        #[cfg(feature = "tls")]
        let tls = self.tls.take();
        #[cfg(feature = "tls")]
        let tls_reload = self.tls_reload;
        let conn_limits = ConnLimits::new(self.max_connections, self.max_connections_per_ip);
        let (trigger, rx) = tokio::sync::watch::channel(false);
        let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
        let shutdown = Shutdown::with_drain(rx, drain_rx);
        let shared = Arc::new(self.into_shared(shutdown.clone()));

        #[allow(unused_mut)]
        let mut tasks: Vec<_> = background
            .into_iter()
            .map(|task| {
                let ctx = Context::new(shared.state.clone(), shutdown.clone());
                tokio::spawn(task(ctx))
            })
            .collect();

        #[cfg(feature = "tls")]
        if let (Some(config), Some(every)) = (tls.clone(), tls_reload) {
            let shutdown = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                while shutdown.sleep(every).await {
                    match config.reload() {
                        Ok(true) => println!("🔐 TLS certificate reloaded"),
                        Ok(false) => {}
                        Err(e) => eprintln!(
                            "TLS certificate reload failed, keeping the previous one: {}",
                            e.message()
                        ),
                    }
                }
            }));
        }

        // every connection holds a clone; the channel closes when all are done
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::pin!(signal);
        let mut stop_at: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                _ = &mut signal, if stop_at.is_none() => {
                    let _ = drain_tx.send(true);
                    if delay.is_zero() {
                        break;
                    }
                    stop_at = Some(tokio::time::Instant::now() + delay);
                }
                _ = tokio::time::sleep_until(stop_at.unwrap_or_else(tokio::time::Instant::now)), if stop_at.is_some() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let Some(guard) = conn_limits.admit(peer.ip()) else {
                            tokio::spawn(limits::refuse(stream));
                            continue;
                        };
                        #[cfg(feature = "tls")]
                        if let Some(config) = tls.clone() {
                            let (shared, shutdown, done) = (shared.clone(), shutdown.clone(), done_tx.clone());
                            tokio::spawn(async move {
                                let _guard = guard;
                                // the handshake runs here, not in the accept loop: a slow
                                // client must not hold up everybody else
                                // scanners, plain HTTP on the TLS port and aborted handshakes just end here
                                if let Ok((tls_stream, client_cert)) = config.accept(stream).await {
                                    let info = ConnInfo { peer, secure: true, client_cert };
                                    serve_connection(tls_stream, info, shared, shutdown, done, _guard).await;
                                }
                            });
                            continue;
                        }
                        let info = ConnInfo { peer, secure: false, client_cert: None };
                        tokio::spawn(serve_connection(
                            stream,
                            info,
                            shared.clone(),
                            shutdown.clone(),
                            done_tx.clone(),
                            guard,
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

    /// Prepare the app for [`crate::testing::TestClient`]: routes and
    /// middleware as in a running server, but no listener and no background tasks
    pub(crate) fn into_in_process(self) -> InProcess {
        let app = self.finalize();
        InProcess {
            shared: Arc::new(app.into_shared(Shutdown::never())),
        }
    }

    fn into_shared(self, shutdown: Shutdown) -> Shared {
        Shared {
            router: self.router,
            middlewares: self.middlewares,
            max_body_size: self.max_body_size,
            body_limits: self.body_limits,
            state: Arc::new(self.state),
            shutdown,
            header_read_timeout: self.header_read_timeout,
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
async fn serve_connection<S>(
    stream: S,
    info: ConnInfo,
    shared: Arc<Shared>,
    shutdown: Shutdown,
    _done: tokio::sync::mpsc::Sender<()>,
    _guard: ConnGuard,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let info = Arc::new(info);
    let service = service_fn(|req| {
        let (shared, info) = (shared.clone(), info.clone());
        async move { Ok::<_, Infallible>(handle_request(req, &info, shared).await) }
    });
    let mut builder = http1::Builder::new();
    if let Some(timeout) = shared.header_read_timeout {
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(timeout);
    }
    let conn = builder.serve_connection(io, service).with_upgrades();
    tokio::pin!(conn);

    let mut draining = false;
    loop {
        tokio::select! {
            result = conn.as_mut() => {
                // a header timeout is routine (slow or idle client), not worth a log line
                if let Err(err) = &result {
                    if !err.is_timeout() {
                        eprintln!("Error serving connection: {:?}", err);
                    }
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
            body_limit: None,
            ws_paths: Vec::new(),
        }
    }

    /// Accept WebSocket connections on `path` below the group prefix; the
    /// group's middleware (authentication, ticket checks, ...) runs before the
    /// upgrade (feature `websocket`)
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
        self.routes.push((
            "GET".to_string(),
            path.to_string(),
            Arc::new(route) as DynHandler,
        ));
        self.ws_paths.push(path.to_string());
        self
    }

    /// Request body limit for this group's routes (`None` = unlimited),
    /// overriding the application default
    pub fn max_body_size(mut self, limit: Option<usize>) -> Self {
        self.body_limit = Some(limit);
        self
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
    conn: &ConnInfo,
    shared: Arc<Shared>,
) -> hyper::Response<crate::response::HyperBody> {
    let path = req.uri().path().to_string();
    let max_body_size = body_limit_for(&shared.body_limits, &path, shared.max_body_size);

    // Convert hyper request to our Request type
    let (parts, body) = req.into_parts();

    // Enforce the body size limit while reading, so it also holds for chunked
    // bodies without a Content-Length
    let body_bytes = match max_body_size {
        Some(max_size) => match read_limited(body, max_size).await {
            Ok(bytes) => bytes,
            Err(too_large) => return too_large.into_hyper(max_size),
        },
        None => match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => bytes::Bytes::new(),
        },
    };

    let mut request = Request::new(parts.method, parts.uri, parts.headers, body_bytes);
    request.extensions = parts.extensions;
    run_pipeline(request, conn, &shared).await.into_hyper()
}

/// An app prepared for in-process calls without sockets, see [`crate::testing`]
pub(crate) struct InProcess {
    shared: Arc<Shared>,
}

impl InProcess {
    pub(crate) async fn call(
        &self,
        request: Request,
        peer: SocketAddr,
        secure: bool,
    ) -> hyper::Response<crate::response::HyperBody> {
        let max = body_limit_for(
            &self.shared.body_limits,
            request.uri.path(),
            self.shared.max_body_size,
        );
        if let Some(max) = max.filter(|max| request.body.len() > *max) {
            return BodyTooLarge.into_hyper(max);
        }
        let conn = ConnInfo {
            peer,
            secure,
            client_cert: None,
        };
        run_pipeline(request, &conn, &self.shared)
            .await
            .into_hyper()
    }
}

/// Route, middleware and error conversion for one fully read request
async fn run_pipeline(mut request: Request, conn: &ConnInfo, shared: &Shared) -> Response {
    let Shared {
        router,
        middlewares,
        state,
        shutdown,
        ..
    } = shared;
    let method = request.method.to_string();
    let is_head = request.method == http::Method::HEAD;
    let path = request.uri.path().to_string();
    request.remote_addr = Some(conn.peer);
    request.secure = conn.secure;
    if let Some(cert) = &conn.client_cert {
        request
            .extensions
            .insert(crate::request::ClientCertificate(cert.clone()));
    }
    request.state = Some(state.clone());
    request.shutdown = Some(shutdown.clone());

    // Match route. A miss still runs the middleware chain (CORS preflight,
    // request ids, logging, security headers) and ends in 404 / 405 / OPTIONS.
    let route = match router.match_route_with_pattern(&method, &path) {
        Ok((handler, params, pattern)) => {
            request.params = params;
            request.matched_path = Some(pattern);
            Route::Found(handler)
        }
        Err(_) => Route::Miss {
            allow: router.allowed_methods(&path),
        },
    };

    let result = if middlewares.is_empty() {
        dispatch(route, request).await
    } else {
        // Route handler (or the miss) runs as the last link of the chain
        let mut chain = middlewares.to_vec();
        let final_handler = Arc::new(move |req: Request, _next: Next| {
            let route = route.clone();
            async move { dispatch(route, req).await }
        });
        chain.push(final_handler as Arc<dyn Middleware>);

        Next::new(Arc::new(chain)).run(request).await
    };

    // Convert Result to Response
    let response = match result {
        Ok(resp) => resp,
        Err(err) => Response::from_error(err),
    };

    // HEAD: same status and headers as GET, no body
    if is_head {
        response.into_head()
    } else {
        response
    }
}

/// Most of a rejected request body that is read and thrown away before the 413
/// is sent. Answering without reading makes the OS reset the connection on
/// close, and clients (notably on Windows) then lose the response.
const DRAIN_CAP: u64 = 8 * 1024 * 1024;
const DRAIN_TIME: Duration = Duration::from_secs(3);

struct BodyTooLarge;

impl BodyTooLarge {
    fn into_hyper(self, max_size: usize) -> hyper::Response<crate::response::HyperBody> {
        Response::from_error(Error::custom(
            413,
            format!("Request body too large (max {} bytes)", max_size),
        ))
        // the connection is not reusable: unread body bytes may remain
        .with_header("connection", "close")
        .into_hyper()
    }
}

/// Collect at most `max` bytes; on overflow discard (a bounded amount of) the rest
async fn read_limited(
    mut body: hyper::body::Incoming,
    max: usize,
) -> std::result::Result<bytes::Bytes, BodyTooLarge> {
    use bytes::BufMut;
    use hyper::body::Body;

    // a declared length is rejected before a single byte is buffered
    if body.size_hint().lower() > max as u64 {
        drain(&mut body, 0).await;
        return Err(BodyTooLarge);
    }

    let mut buf = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return Ok(bytes::Bytes::new());
        };
        if let Ok(data) = frame.into_data() {
            if buf.len() + data.len() > max {
                drain(&mut body, (buf.len() + data.len()) as u64).await;
                return Err(BodyTooLarge);
            }
            buf.put(data);
        }
    }
    Ok(buf.freeze())
}

/// Read and drop up to [`DRAIN_CAP`] bytes (counting `already` read), for at most [`DRAIN_TIME`]
async fn drain(body: &mut hyper::body::Incoming, already: u64) {
    let _ = tokio::time::timeout(DRAIN_TIME, async {
        let mut seen = already;
        while seen < DRAIN_CAP {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        seen += data.len() as u64;
                    }
                }
                _ => return,
            }
        }
    })
    .await;
}

/// Limit for `path`: the longest matching prefix rule, else the default
fn body_limit_for(
    rules: &[(String, Option<usize>)],
    path: &str,
    default: Option<usize>,
) -> Option<usize> {
    rules
        .iter()
        .filter(|(prefix, _)| {
            prefix.is_empty()
                || path == prefix
                || path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, limit)| *limit)
        .unwrap_or(default)
}

/// Outcome of routing
#[derive(Clone)]
enum Route {
    Found(DynHandler),
    /// No handler for this method; `allow` is empty when the path is unknown
    Miss {
        allow: Vec<&'static str>,
    },
}

async fn dispatch(route: Route, req: Request) -> Result<Response> {
    match route {
        Route::Found(handler) => handler.call(req).await,
        Route::Miss { allow } if allow.is_empty() => Err(Error::not_found("Route not found")),
        Route::Miss { allow } => {
            let allow = allow.join(", ");
            if req.method == http::Method::OPTIONS {
                // automatic OPTIONS for known paths (CORS preflights are answered
                // earlier, by the cors middleware)
                Ok(Response::new(http::StatusCode::NO_CONTENT).with_header("allow", allow))
            } else {
                Ok(
                    Response::from_error(Error::custom(405, "Method not allowed"))
                        .with_header("allow", allow),
                )
            }
        }
    }
}
