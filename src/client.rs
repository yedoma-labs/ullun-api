//! Outbound HTTP client
//!
//! Services call other services. [`HttpClient`] is a small client for that with
//! the settings production calls need and ad-hoc code forgets:
//!
//! * timeouts for connecting and for the whole attempt, and a cap on the
//!   response size
//! * connection pooling (keep-alive) per host
//! * retries with exponential backoff and jitter for connection failures and
//!   `502/503/504/429` (honouring `Retry-After`), only for requests that are safe
//!   to repeat: idempotent methods or requests carrying an `Idempotency-Key`
//! * redirects (followed, up to a limit; credentials are dropped when the
//!   target is another origin)
//! * an SSRF guard ([`HttpClient::public_only`]): every connection, including
//!   those after a redirect, goes to a vetted public address
//! * HTTPS with the Mozilla root certificates (feature `client-tls`)
//!
//! ```no_run
//! use std::time::Duration;
//! use ullun::client::HttpClient;
//!
//! # async fn run() -> Result<(), ullun::client::ClientError> {
//! let client = HttpClient::new()
//!     .timeout(Duration::from_secs(5))
//!     .retries(2)
//!     .public_only();
//! let res = client.get("http://example.com/api/items?limit=10").send().await?;
//! if res.status() == 200 {
//!     let items: serde_json::Value = res.json()?;
//!     println!("{items}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`ClientError`] converts into [`Error`](crate::error::Error) (`504` for
//! timeouts, `502` otherwise), so handlers can use `?` on client calls.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Why a client call failed
#[derive(Debug)]
#[non_exhaustive]
pub enum ClientError {
    /// The URL is not a usable `http://` or `https://` URL
    InvalidUrl(String),
    /// Name resolution or connecting failed
    Connect(String),
    /// The TLS handshake failed or `https` is not available
    Tls(String),
    /// The target resolved to a non-public address (with [`HttpClient::public_only`])
    Blocked(IpAddr),
    /// The attempt took longer than the timeout
    Timeout,
    /// The peer did not speak valid HTTP or closed the connection early
    Protocol(String),
    /// The response body exceeds [`HttpClient::max_response_bytes`]
    TooLarge(usize),
    /// More redirects than [`HttpClient::max_redirects`]
    TooManyRedirects,
    /// The response body is not the expected JSON / UTF-8
    Decode(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::InvalidUrl(u) => write!(f, "invalid URL: {u}"),
            ClientError::Connect(e) => write!(f, "connection failed: {e}"),
            ClientError::Tls(e) => write!(f, "TLS error: {e}"),
            ClientError::Blocked(ip) => write!(f, "target address {ip} is not public"),
            ClientError::Timeout => f.write_str("request timed out"),
            ClientError::Protocol(e) => write!(f, "invalid response: {e}"),
            ClientError::TooLarge(max) => write!(f, "response larger than {max} bytes"),
            ClientError::TooManyRedirects => f.write_str("too many redirects"),
            ClientError::Decode(e) => write!(f, "cannot decode response: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<ClientError> for crate::error::Error {
    fn from(e: ClientError) -> Self {
        let status = if matches!(e, ClientError::Timeout) {
            504
        } else {
            502
        };
        crate::error::Error::custom(status, format!("Upstream request failed: {e}"))
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

type Sender = SendRequest<Full<Bytes>>;

#[derive(Hash, PartialEq, Eq, Clone)]
struct PoolKey {
    https: bool,
    host: String,
    port: u16,
}

struct Idle {
    sender: Sender,
    since: Instant,
}

struct Shared {
    pool: Mutex<HashMap<PoolKey, Vec<Idle>>>,
    #[cfg(feature = "client-tls")]
    tls: tokio_rustls::TlsConnector,
}

/// HTTP client, see the [module documentation](self). Cloning shares the connection pool.
#[derive(Clone)]
pub struct HttpClient {
    shared: Arc<Shared>,
    timeout: Duration,
    connect_timeout: Duration,
    max_response: usize,
    max_redirects: usize,
    retries: u32,
    backoff: Duration,
    public_only: bool,
    user_agent: String,
    headers: HeaderMap,
    idle_timeout: Duration,
    max_idle_per_host: usize,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                pool: Mutex::new(HashMap::new()),
                #[cfg(feature = "client-tls")]
                tls: tls_connector(&[]).expect("default TLS configuration"),
            }),
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_response: 10 * 1024 * 1024,
            max_redirects: 5,
            retries: 0,
            backoff: Duration::from_millis(200),
            public_only: false,
            user_agent: concat!("ullun-api/", env!("CARGO_PKG_VERSION")).to_string(),
            headers: HeaderMap::new(),
            idle_timeout: Duration::from_secs(30),
            max_idle_per_host: 8,
        }
    }

    /// Longest a single attempt (connect, send, read the whole response) may take (default 30 s)
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Longest name resolution + TCP/TLS connect may take (default 10 s)
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Largest response body accepted (default 10 MiB)
    pub fn max_response_bytes(mut self, max: usize) -> Self {
        self.max_response = max;
        self
    }

    /// Redirects followed per call (default 5, 0 returns the 3xx response itself)
    pub fn max_redirects(mut self, max: usize) -> Self {
        self.max_redirects = max;
        self
    }

    /// Extra attempts after a failure that is safe to repeat (default 0)
    pub fn retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    /// First retry delay, doubled each time with jitter (default 200 ms)
    pub fn retry_backoff(mut self, base: Duration) -> Self {
        self.backoff = base;
        self
    }

    /// Only connect to public addresses: loopback, private, link-local and
    /// cloud-metadata targets are refused with [`ClientError::Blocked`]. Use it
    /// whenever the URL comes from a user (webhooks, link previews, imports).
    pub fn public_only(mut self) -> Self {
        self.public_only = true;
        self
    }

    pub fn user_agent(mut self, agent: &str) -> Self {
        self.user_agent = agent.to_string();
        self
    }

    /// Header sent with every request (unless the request sets it itself)
    pub fn default_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            self.headers.insert(n, v);
        }
        self
    }

    /// Keep idle connections this long for reuse (default 30 s, zero disables pooling)
    pub fn pool_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Trust these PEM root certificates in addition to the built-in ones, e.g.
    /// a private CA (feature `client-tls`)
    #[cfg(feature = "client-tls")]
    pub fn add_root_certificates_pem(mut self, pem: &[u8]) -> Result<Self, ClientError> {
        let connector = tls_connector(&[pem])?;
        let shared = Shared {
            pool: Mutex::new(HashMap::new()),
            tls: connector,
        };
        self.shared = Arc::new(shared);
        Ok(self)
    }

    pub fn request(&self, method: Method, url: &str) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            url: url.to_string(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            timeout: None,
            error: None,
        }
    }

    pub fn get(&self, url: &str) -> RequestBuilder {
        self.request(Method::GET, url)
    }
    pub fn post(&self, url: &str) -> RequestBuilder {
        self.request(Method::POST, url)
    }
    pub fn put(&self, url: &str) -> RequestBuilder {
        self.request(Method::PUT, url)
    }
    pub fn patch(&self, url: &str) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }
    pub fn delete(&self, url: &str) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }
    pub fn head(&self, url: &str) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }
}

/// A request under construction, created by the verbs of [`HttpClient`]
pub struct RequestBuilder {
    client: HttpClient,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Bytes,
    timeout: Option<Duration>,
    error: Option<ClientError>,
}

impl RequestBuilder {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(n), Ok(v)) => {
                self.headers.insert(n, v);
            }
            _ => self.error = Some(ClientError::InvalidUrl(format!("invalid header {name}"))),
        }
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    pub fn json<T: Serialize>(mut self, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => {
                self.body = Bytes::from(body);
                self.header("content-type", "application/json")
            }
            Err(e) => {
                self.error = Some(ClientError::Decode(e.to_string()));
                self
            }
        }
    }

    pub fn form<T: Serialize>(mut self, value: &T) -> Self {
        match serde_urlencoded::to_string(value) {
            Ok(body) => {
                self.body = Bytes::from(body);
                self.header("content-type", "application/x-www-form-urlencoded")
            }
            Err(e) => {
                self.error = Some(ClientError::Decode(e.to_string()));
                self
            }
        }
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// Append query parameters (percent-encoded)
    pub fn query<T: Serialize>(mut self, params: &T) -> Self {
        match serde_urlencoded::to_string(params) {
            Ok(q) if !q.is_empty() => {
                self.url
                    .push(if self.url.contains('?') { '&' } else { '?' });
                self.url.push_str(&q);
            }
            Ok(_) => {}
            Err(e) => self.error = Some(ClientError::Decode(e.to_string())),
        }
        self
    }

    /// Per-attempt timeout for this request only
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub async fn send(self) -> Result<ClientResponse, ClientError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let c = &self.client;
        let timeout = self.timeout.unwrap_or(c.timeout);
        let repeatable = is_repeatable(&self.method, &self.headers);
        let mut attempt = 0;
        loop {
            let outcome = match tokio::time::timeout(timeout, c.run(&self)).await {
                Ok(r) => r,
                Err(_) => Err(ClientError::Timeout),
            };
            let retry_after = match &outcome {
                Ok(res) if retryable_status(res.status) => Some(res.retry_after()),
                Ok(_) => None,
                Err(e) if retryable_error(e) => Some(None),
                Err(_) => None,
            };
            match retry_after {
                Some(hint) if repeatable && attempt < c.retries => {
                    let wait = hint.unwrap_or_else(|| c.delay(attempt));
                    // do not sleep past a sensible bound for a server's Retry-After
                    tokio::time::sleep(wait.min(Duration::from_secs(10))).await;
                    attempt += 1;
                }
                _ => return outcome,
            }
        }
    }
}

fn is_repeatable(method: &Method, headers: &HeaderMap) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS
    ) || headers.contains_key("idempotency-key")
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504)
}

fn retryable_error(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Connect(_) | ClientError::Timeout | ClientError::Protocol(_)
    )
}

impl HttpClient {
    /// Exponential backoff with up to 50 % jitter
    fn delay(&self, attempt: u32) -> Duration {
        let base = self.backoff.saturating_mul(1 << attempt.min(8));
        let jitter = crate::security::random_bytes(1)
            .ok()
            .and_then(|b| b.first().copied())
            .unwrap_or(0) as f64
            / 255.0
            * 0.5;
        base.mul_f64(1.0 + jitter)
    }

    /// One attempt, following redirects
    async fn run(&self, req: &RequestBuilder) -> Result<ClientResponse, ClientError> {
        let mut url = req.url.clone();
        let mut method = req.method.clone();
        let mut body = req.body.clone();
        let mut headers = req.headers.clone();
        let origin = origin_of(&req.url)?;
        for _ in 0..=self.max_redirects {
            let uri: Uri = url
                .parse()
                .map_err(|_| ClientError::InvalidUrl(url.clone()))?;
            let res = self.exchange(&uri, &method, &headers, &body).await?;
            let status = res.status;
            let location = res
                .headers
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok());
            if self.max_redirects == 0 || !status.is_redirection() || location.is_none() {
                return Ok(res);
            }
            let next = resolve_location(&uri, location.unwrap_or_default())
                .ok_or_else(|| ClientError::Protocol("bad redirect location".into()))?;
            // 301/302/303 turn into GET without a body; 307/308 repeat the request
            if matches!(status.as_u16(), 301..=303) && method != Method::HEAD {
                method = Method::GET;
                body = Bytes::new();
                headers.remove(http::header::CONTENT_TYPE);
                headers.remove(http::header::CONTENT_LENGTH);
            }
            if origin_of(&next)? != origin {
                // never hand credentials to another origin
                headers.remove(http::header::AUTHORIZATION);
                headers.remove(http::header::COOKIE);
            }
            url = next;
        }
        Err(ClientError::TooManyRedirects)
    }

    async fn exchange(
        &self,
        uri: &Uri,
        method: &Method,
        extra: &HeaderMap,
        body: &Bytes,
    ) -> Result<ClientResponse, ClientError> {
        let (https, host, port) = target(uri)?;
        let key = PoolKey {
            https,
            host: host.clone(),
            port,
        };
        let path = uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();

        let build = || -> Result<http::Request<Full<Bytes>>, ClientError> {
            let mut b = http::Request::builder().method(method.clone()).uri(&path);
            let h = b.headers_mut().expect("builder has headers");
            let host_value = if (https && port == 443) || (!https && port == 80) {
                host.clone()
            } else {
                format!("{host}:{port}")
            };
            h.insert(
                http::header::HOST,
                HeaderValue::from_str(&host_value)
                    .map_err(|_| ClientError::InvalidUrl(uri.to_string()))?,
            );
            if let Ok(agent) = HeaderValue::from_str(&self.user_agent) {
                h.insert(http::header::USER_AGENT, agent);
            }
            for (n, v) in self.headers.iter().chain(extra.iter()) {
                h.insert(n.clone(), v.clone());
            }
            if !body.is_empty() || matches!(*method, Method::POST | Method::PUT | Method::PATCH) {
                h.insert(http::header::CONTENT_LENGTH, body.len().into());
            }
            b.body(Full::new(body.clone()))
                .map_err(|e| ClientError::Protocol(e.to_string()))
        };

        // a pooled connection may have been closed by the peer meanwhile: one
        // fresh attempt is then always safe if nothing was received
        let mut sender = self.checkout(&key);
        let mut response = None;
        if let Some(s) = sender.as_mut() {
            match s.send_request(build()?).await {
                Ok(r) => response = Some(r),
                Err(_) => sender = None,
            }
        }
        let response = match response {
            Some(r) => r,
            None => {
                let mut fresh = self.connect(&key).await?;
                let r = fresh
                    .send_request(build()?)
                    .await
                    .map_err(|e| ClientError::Protocol(e.to_string()))?;
                sender = Some(fresh);
                r
            }
        };

        let (parts, incoming) = response.into_parts();
        let bytes = read_limited(incoming, self.max_response).await?;
        let keep = parts.version == http::Version::HTTP_11
            && !parts
                .headers
                .get(http::header::CONNECTION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        if let (true, Some(s)) = (keep, sender) {
            self.checkin(key, s);
        }
        Ok(ClientResponse {
            status: parts.status,
            headers: parts.headers,
            body: bytes,
        })
    }

    fn checkout(&self, key: &PoolKey) -> Option<Sender> {
        let mut pool = self.shared.pool.lock().unwrap_or_else(|e| e.into_inner());
        let list = pool.get_mut(key)?;
        while let Some(idle) = list.pop() {
            if !idle.sender.is_closed() && idle.since.elapsed() < self.idle_timeout {
                return Some(idle.sender);
            }
        }
        None
    }

    fn checkin(&self, key: PoolKey, sender: Sender) {
        if self.idle_timeout.is_zero() || sender.is_closed() {
            return;
        }
        let mut pool = self.shared.pool.lock().unwrap_or_else(|e| e.into_inner());
        let list = pool.entry(key).or_default();
        list.retain(|i| !i.sender.is_closed() && i.since.elapsed() < self.idle_timeout);
        if list.len() < self.max_idle_per_host {
            list.push(Idle {
                sender,
                since: Instant::now(),
            });
        }
    }

    async fn connect(&self, key: &PoolKey) -> Result<Sender, ClientError> {
        #[cfg(not(feature = "client-tls"))]
        if key.https {
            return Err(ClientError::Tls(
                "https needs the `client-tls` feature of ullun-api".into(),
            ));
        }
        let connect = async {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((key.host.as_str(), key.port))
                .await
                .map_err(|e| ClientError::Connect(e.to_string()))?
                .collect();
            if addrs.is_empty() {
                return Err(ClientError::Connect("name resolution failed".into()));
            }
            // every answer must be public, not just the one used, and we connect
            // to the vetted address itself (no second lookup to rebind)
            if self.public_only {
                if let Some(bad) = addrs
                    .iter()
                    .find(|a| !crate::security::is_public_ip(a.ip()))
                {
                    return Err(ClientError::Blocked(bad.ip()));
                }
            }
            // try the addresses in order: "localhost" may list ::1 first
            let mut last = String::new();
            let mut connected = None;
            for addr in addrs {
                match TcpStream::connect(addr).await {
                    Ok(tcp) => {
                        connected = Some(tcp);
                        break;
                    }
                    Err(e) => last = e.to_string(),
                }
            }
            let tcp = connected.ok_or(ClientError::Connect(last))?;
            let _ = tcp.set_nodelay(true);
            let io: Box<dyn Io> = if key.https {
                self.tls(tcp, &key.host).await?
            } else {
                Box::new(tcp)
            };
            let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
                .await
                .map_err(|e| ClientError::Protocol(e.to_string()))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            Ok::<_, ClientError>(sender)
        };
        tokio::time::timeout(self.connect_timeout, connect)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    #[cfg(feature = "client-tls")]
    async fn tls(&self, tcp: TcpStream, host: &str) -> Result<Box<dyn Io>, ClientError> {
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| ClientError::Tls(format!("invalid server name {host}")))?;
        let stream = self
            .shared
            .tls
            .connect(name, tcp)
            .await
            .map_err(|e| ClientError::Tls(e.to_string()))?;
        Ok(Box::new(stream))
    }

    #[cfg(not(feature = "client-tls"))]
    async fn tls(&self, _tcp: TcpStream, _host: &str) -> Result<Box<dyn Io>, ClientError> {
        Err(ClientError::Tls(
            "https needs the `client-tls` feature of ullun-api".into(),
        ))
    }
}

#[cfg(feature = "client-tls")]
fn tls_connector(extra_pem: &[&[u8]]) -> Result<tokio_rustls::TlsConnector, ClientError> {
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::CertificateDer;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for pem in extra_pem {
        let mut added = 0;
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|e| ClientError::Tls(e.to_string()))?;
            roots
                .add(cert)
                .map_err(|e| ClientError::Tls(e.to_string()))?;
            added += 1;
        }
        if added == 0 {
            return Err(ClientError::Tls("no certificate in the PEM data".into()));
        }
    }
    let mut config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| ClientError::Tls(e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

async fn read_limited(mut body: Incoming, max: usize) -> Result<Bytes, ClientError> {
    use bytes::BufMut;
    use hyper::body::Body;
    if body.size_hint().lower() > max as u64 {
        return Err(ClientError::TooLarge(max));
    }
    let mut out = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| ClientError::Protocol(e.to_string()))?;
        if let Some(data) = frame.data_ref() {
            if out.len() + data.len() > max {
                return Err(ClientError::TooLarge(max));
            }
            out.put_slice(data);
        }
    }
    Ok(out.freeze())
}

/// (https, host, port) of an absolute `http(s)` URL
fn target(uri: &Uri) -> Result<(bool, String, u16), ClientError> {
    let https = match uri.scheme_str() {
        Some("http") => false,
        Some("https") => true,
        _ => return Err(ClientError::InvalidUrl(uri.to_string())),
    };
    let host = uri
        .host()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| ClientError::InvalidUrl(uri.to_string()))?;
    // IPv6 literals come bracketed
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    Ok((https, host, port))
}

fn origin_of(url: &str) -> Result<(bool, String, u16), ClientError> {
    let uri: Uri = url
        .parse()
        .map_err(|_| ClientError::InvalidUrl(url.to_string()))?;
    target(&uri)
}

/// Absolute URL for a (possibly relative) `Location` value
fn resolve_location(base: &Uri, location: &str) -> Option<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Some(location.to_string());
    }
    let scheme = base.scheme_str()?;
    let authority = base.authority()?;
    if location.starts_with('/') {
        return Some(format!("{scheme}://{authority}{location}"));
    }
    // relative to the directory of the current path
    let dir = base.path().rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    Some(format!("{scheme}://{authority}{dir}/{location}"))
}

/// What the server answered
#[derive(Debug, Clone)]
pub struct ClientResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl ClientResponse {
    pub fn status(&self) -> u16 {
        self.status.as_u16()
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Header value, empty when absent or not text
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    pub fn bytes(&self) -> &Bytes {
        &self.body
    }

    pub fn text(&self) -> Result<String, ClientError> {
        String::from_utf8(self.body.to_vec()).map_err(|e| ClientError::Decode(e.to_string()))
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, ClientError> {
        serde_json::from_slice(&self.body).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Seconds from a numeric `Retry-After` header
    fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")
            .trim()
            .parse::<u64>()
            .ok()
            .map(Duration::from_secs)
    }
}
