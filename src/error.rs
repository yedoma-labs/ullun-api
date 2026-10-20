//! Error types and Result alias

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// One invalid field of a request, reported by [`Error::Validation`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

/// Framework error type that maps to HTTP status codes
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// 400 Bad Request
    BadRequest(String),
    /// 401 Unauthorized
    Unauthorized(String),
    /// 403 Forbidden
    Forbidden(String),
    /// 404 Not Found
    NotFound(String),
    /// 500 Internal Server Error
    Internal(String),
    /// 422 Unprocessable Content with one entry per invalid field, rendered as
    /// `{"error": "Validation failed", "status": 422, "fields": {"title": ["..."]}}`.
    /// See [`Validator`](crate::validate::Validator).
    Validation(Vec<FieldError>),
    /// Custom status code + message
    Custom(u16, String),
}

impl Error {
    pub fn status_code(&self) -> u16 {
        match self {
            Error::BadRequest(_) => 400,
            Error::Unauthorized(_) => 401,
            Error::Forbidden(_) => 403,
            Error::NotFound(_) => 404,
            Error::Internal(_) => 500,
            Error::Validation(_) => 422,
            Error::Custom(code, _) => *code,
        }
    }

    /// The status as an [`http::StatusCode`] (invalid custom codes become 500)
    pub fn status(&self) -> http::StatusCode {
        http::StatusCode::from_u16(self.status_code())
            .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR)
    }

    pub fn message(&self) -> &str {
        match self {
            Error::Validation(_) => "Validation failed",
            Error::BadRequest(msg)
            | Error::Unauthorized(msg)
            | Error::Forbidden(msg)
            | Error::NotFound(msg)
            | Error::Internal(msg)
            | Error::Custom(_, msg) => msg,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for Error {}

// Convenience constructors
impl Error {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Error::BadRequest(msg.into())
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Error::Unauthorized(msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Error::Forbidden(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Error::NotFound(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Error::Internal(msg.into())
    }

    /// 422 with the given invalid fields
    pub fn validation(fields: Vec<FieldError>) -> Self {
        Error::Validation(fields)
    }

    /// The invalid fields of a [`Error::Validation`], empty for other errors
    pub fn fields(&self) -> &[FieldError] {
        match self {
            Error::Validation(fields) => fields,
            _ => &[],
        }
    }

    pub fn custom(status: u16, msg: impl Into<String>) -> Self {
        Error::Custom(status, msg.into())
    }
}
