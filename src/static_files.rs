//! Static file serving

use crate::error::{Error, Result};
use crate::handler::Handler;
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::fs;

pub struct StaticFileHandler {
    root_dir: PathBuf,
}

impl StaticFileHandler {
    pub fn new(root_dir: &str) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir),
        }
    }

    async fn serve_file(&self, file_path: &str) -> Result<Response> {
        // Sanitize path to prevent directory traversal
        let file_path = file_path.trim_start_matches('/');
        let full_path = self.root_dir.join(file_path);

        // Ensure the path is within root_dir
        let canonical_root = fs::canonicalize(&self.root_dir)
            .await
            .map_err(|_| Error::not_found("Static directory not found"))?;

        // Check if file exists first
        if !full_path.exists() {
            return Err(Error::not_found("File not found"));
        }

        let canonical_path = fs::canonicalize(&full_path)
            .await
            .map_err(|_| Error::not_found("File not found"))?;

        if !canonical_path.starts_with(&canonical_root) {
            return Err(Error::forbidden("Access denied"));
        }

        // Check if it's a directory
        let metadata = fs::metadata(&canonical_path)
            .await
            .map_err(|_| Error::not_found("File not found"))?;

        if metadata.is_dir() {
            // Try to serve index.html
            let index_path = canonical_path.join("index.html");
            if index_path.exists() {
                return self.serve_file_content(&index_path).await;
            }
            return Err(Error::forbidden("Directory listing not allowed"));
        }

        self.serve_file_content(&canonical_path).await
    }

    async fn serve_file_content(&self, path: &Path) -> Result<Response> {
        let content = fs::read(path)
            .await
            .map_err(|_| Error::internal("Failed to read file"))?;

        let content_type = guess_content_type(path);

        Ok(Response::new(http::StatusCode::OK)
            .with_header("content-type", content_type)
            .with_header("cache-control", "public, max-age=3600")
            .with_body(content))
    }
}

#[async_trait]
impl Handler for StaticFileHandler {
    async fn call(&self, req: Request) -> Result<Response> {
        let filepath = req.params.get("filepath").unwrap_or("");
        self.serve_file(filepath).await
    }
}

fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_type_detection() {
        assert_eq!(guess_content_type(Path::new("test.html")), "text/html; charset=utf-8");
        assert_eq!(guess_content_type(Path::new("test.css")), "text/css; charset=utf-8");
        assert_eq!(guess_content_type(Path::new("test.js")), "application/javascript; charset=utf-8");
        assert_eq!(guess_content_type(Path::new("test.json")), "application/json");
        assert_eq!(guess_content_type(Path::new("test.png")), "image/png");
        assert_eq!(guess_content_type(Path::new("unknown")), "application/octet-stream");
    }
}
