//! Response types and builders

use crate::cookies::Cookie;
use crate::error::Error;
use bytes::Bytes;
use http::StatusCode;
use serde::Serialize;

/// HTTP Response wrapper
pub struct Response {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    cookies: Vec<Cookie>,
    stream: Option<tokio::sync::mpsc::Receiver<Bytes>>,
}

/// Body type of everything sent to hyper: buffered or streamed
pub(crate) type HyperBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

/// Chunks arriving through a channel, ended when every sender is dropped
struct ChannelBody {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        self.rx
            .poll_recv(cx)
            .map(|chunk| chunk.map(|b| Ok(hyper::body::Frame::data(b))))
    }
}

impl Response {
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
            cookies: Vec::new(),
            stream: None,
        }
    }

    /// A response whose body is produced while it is being sent
    ///
    /// Returns the sender for the body chunks and the response; set status and
    /// headers on the response as usual. The body ends when the last
    /// [`BodySender`](crate::stream::BodySender) is dropped, and `send` fails
    /// with [`Disconnected`](crate::stream::Disconnected) once the client is gone.
    /// Middleware that rewrites `body` (compression, ETags) leaves streams alone.
    ///
    /// ```
    /// use ullun::prelude::*;
    ///
    /// async fn countdown(_req: Request) -> Result<Response> {
    ///     let (tx, res) = Response::stream();
    ///     tokio::spawn(async move {
    ///         for n in (1..=3).rev() {
    ///             if tx.send(format!("{n}\n")).await.is_err() {
    ///                 return; // client went away
    ///             }
    ///         }
    ///     });
    ///     Ok(res.with_header("content-type", "text/plain"))
    /// }
    /// ```
    pub fn stream() -> (crate::stream::BodySender, Self) {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut res = Self::ok();
        res.stream = Some(rx);
        (crate::stream::BodySender::new(tx), res)
    }

    /// The response a `HEAD` request gets: same status and headers, no body.
    /// `content-length` is set from the buffered body unless already present.
    pub(crate) fn into_head(mut self) -> Self {
        let bodiless = self.status.is_informational()
            || self.status == StatusCode::NO_CONTENT
            || self.status == StatusCode::NOT_MODIFIED;
        if self.stream.is_none()
            && !bodiless
            && !self.headers.contains_key(http::header::CONTENT_LENGTH)
        {
            if let Ok(len) = http::HeaderValue::from_str(&self.body.len().to_string()) {
                self.headers.insert(http::header::CONTENT_LENGTH, len);
            }
        }
        self.body = Bytes::new();
        self.stream = None;
        self
    }

    /// Whether `Response::cookie` was used
    pub(crate) fn has_cookies(&self) -> bool {
        !self.cookies.is_empty()
    }

    /// Whether the body is streamed instead of held in `body`
    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// Set a cookie
    pub fn cookie(mut self, cookie: Cookie) -> Self {
        self.cookies.push(cookie);
        self
    }

    pub fn ok() -> Self {
        Self::new(StatusCode::OK)
    }

    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        let value = value.into();

        // Validate and insert header, skip invalid ones
        if let (Ok(header_name), Ok(header_value)) = (
            http::HeaderName::from_bytes(key.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            self.headers.insert(header_name, header_value);
        } else {
            eprintln!("Warning: Invalid header {}={}", key, value);
        }
        self
    }

    pub fn json<T: Serialize>(data: T) -> Self {
        match serde_json::to_vec(&data) {
            Ok(body) => Self::ok()
                .with_header("content-type", "application/json")
                .with_body(body),
            Err(e) => {
                eprintln!("JSON serialization error: {}", e);
                Self::new(StatusCode::INTERNAL_SERVER_ERROR)
                    .with_header("content-type", "application/json")
                    .with_body("{\"error\":\"Internal serialization error\"}")
            }
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::ok()
            .with_header("content-type", "text/plain")
            .with_body(text.into())
    }

    /// `200 OK` with an HTML body
    pub fn html(html: impl Into<String>) -> Self {
        Self::ok()
            .with_header("content-type", "text/html; charset=utf-8")
            .with_body(html.into())
    }

    /// `303 See Other`, the right answer after a form POST (Post/Redirect/Get)
    pub fn redirect(location: impl Into<String>) -> Self {
        Self::new(StatusCode::SEE_OTHER).with_header("location", location)
    }

    /// `308 Permanent Redirect`, keeps method and body
    pub fn redirect_permanent(location: impl Into<String>) -> Self {
        Self::new(StatusCode::PERMANENT_REDIRECT).with_header("location", location)
    }

    pub fn from_error(error: Error) -> Self {
        let status = error.status();
        let mut body = serde_json::json!({
            "error": error.message(),
            "status": error.status_code(),
        });
        if let Error::Validation(fields) = &error {
            // field -> list of messages, in the order they were found
            let mut by_field = serde_json::Map::new();
            for f in fields {
                if let Some(list) = by_field
                    .entry(f.field.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()))
                    .as_array_mut()
                {
                    list.push(serde_json::Value::String(f.message.clone()));
                }
            }
            body["fields"] = serde_json::Value::Object(by_field);
        }
        let body_bytes =
            serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"error\":\"Unknown error\"}".to_vec());
        Self::new(status)
            .with_header("content-type", "application/json")
            .with_body(body_bytes)
    }

    pub(crate) fn into_hyper(self) -> hyper::Response<HyperBody> {
        use http_body_util::BodyExt;
        let mut builder = hyper::Response::builder().status(self.status);

        for (key, value) in self.headers.iter() {
            builder = builder.header(key, value);
        }

        // Add Set-Cookie headers
        for cookie in self.cookies {
            builder = builder.header("set-cookie", cookie.to_header_value());
        }

        let body: HyperBody = match self.stream {
            Some(rx) => ChannelBody { rx }.boxed(),
            None => http_body_util::Full::new(self.body)
                .map_err(|never| match never {})
                .boxed(),
        };
        builder.body(body).unwrap_or_else(|e| {
            eprintln!("Failed to build response: {}", e);
            hyper::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(
                    http_body_util::Full::new(Bytes::from("Internal Server Error"))
                        .map_err(|never| match never {})
                        .boxed(),
                )
                .unwrap()
        })
    }
}

/// JSON response helper
pub struct Json<T>(pub T);

impl<T: Serialize> From<Json<T>> for Response {
    fn from(json: Json<T>) -> Self {
        Response::json(json.0)
    }
}
