//! Response types and builders

use crate::error::Error;
use bytes::Bytes;
use http::StatusCode;
use serde::Serialize;

/// HTTP Response wrapper
pub struct Response {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Response {
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    pub fn ok() -> Self {
        Self::new(StatusCode::OK)
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

    pub fn from_error(error: Error) -> Self {
        let status =
            StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::json!({
            "error": error.message(),
            "status": error.status_code(),
        });
        let body_bytes =
            serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"error\":\"Unknown error\"}".to_vec());
        Self::new(status)
            .with_header("content-type", "application/json")
            .with_body(body_bytes)
    }

    pub(crate) fn into_hyper(self) -> hyper::Response<http_body_util::Full<Bytes>> {
        let mut builder = hyper::Response::builder().status(self.status);

        for (key, value) in self.headers.iter() {
            builder = builder.header(key, value);
        }

        builder
            .body(http_body_util::Full::new(self.body))
            .unwrap_or_else(|e| {
                eprintln!("Failed to build response: {}", e);
                hyper::Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(http_body_util::Full::new(Bytes::from(
                        "Internal Server Error",
                    )))
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
