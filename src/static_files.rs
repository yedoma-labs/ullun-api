//! Static file serving

use crate::error::{Error, Result};
use crate::files::{serve_file, FileOptions};
use crate::handler::Handler;
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::fs;

/// Serves a directory: files, `index.html` for directories, ranges, conditional
/// requests and streaming for large files (see [`crate::files`]).
///
/// Dotfiles (`.env`, `.git/`, ...) are never served, and requests that try to
/// leave the directory (`..`, absolute paths, symlinks pointing outside) get `404`.
pub struct StaticFileHandler {
    root_dir: PathBuf,
}

impl StaticFileHandler {
    pub fn new(root_dir: &str) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir),
        }
    }

    async fn serve(&self, req: &Request, file_path: &str) -> Result<Response> {
        let not_found = || Error::not_found("File not found");

        // %20 etc. in file names; decoded before validating, so %2e%2e is caught too
        let decoded = urlencoding::decode(file_path).map_err(|_| not_found())?;
        let mut relative = PathBuf::new();
        for segment in decoded.split('/').filter(|s| !s.is_empty()) {
            let unsafe_segment = segment.starts_with('.')
                || segment.contains(['\\', '\0', ':'])
                || segment.chars().any(char::is_control);
            if unsafe_segment {
                return Err(not_found());
            }
            relative.push(segment);
        }

        let canonical_root = fs::canonicalize(&self.root_dir)
            .await
            .map_err(|_| Error::not_found("Static directory not found"))?;
        let mut canonical = fs::canonicalize(canonical_root.join(&relative))
            .await
            .map_err(|_| not_found())?;
        // 404 rather than 403: do not reveal whether the target exists
        if !canonical.starts_with(&canonical_root) {
            return Err(not_found());
        }

        let meta = fs::metadata(&canonical).await.map_err(|_| not_found())?;
        if meta.is_dir() {
            let index = canonical.join("index.html");
            if fs::metadata(&index).await.is_ok_and(|m| m.is_file()) {
                canonical = index;
            } else {
                return Err(Error::forbidden("Directory listing not allowed"));
            }
        }

        serve_file(
            req,
            &canonical,
            &FileOptions::new().cache_control("public, max-age=3600"),
        )
        .await
    }
}

#[async_trait]
impl Handler for StaticFileHandler {
    async fn call(&self, req: Request) -> Result<Response> {
        let filepath = req.params.get("filepath").unwrap_or("").to_string();
        self.serve(&req, &filepath).await
    }
}

#[cfg(test)]
mod tests {
    use crate::files::content_type_for;
    use std::path::Path;

    #[test]
    fn test_content_type_detection() {
        assert_eq!(
            content_type_for(Path::new("test.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("test.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("test.js")),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for(Path::new("test.json")), "application/json");
        assert_eq!(content_type_for(Path::new("test.png")), "image/png");
        assert_eq!(
            content_type_for(Path::new("unknown")),
            "application/octet-stream"
        );
    }
}
