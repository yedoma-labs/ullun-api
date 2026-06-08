//! Error types and Result alias

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Framework error type that maps to HTTP status codes
#[derive(Debug)]
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
            Error::Custom(code, _) => *code,
        }
    }

    pub fn message(&self) -> &str {
        match self {
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

    pub fn custom(status: u16, msg: impl Into<String>) -> Self {
        Error::Custom(status, msg.into())
    }
}
