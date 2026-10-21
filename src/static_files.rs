//! Static file serving

use crate::error::{Error, Result};
use crate::files::{serve_file, FileOptions};
use crate::handler::Handler;
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::fs;

/// How a directory is served, see [`App::serve_static_with`](crate::app::App::serve_static_with)
///
/// ```
/// use ullun::static_files::StaticOptions;
///
/// // a single-page app built by a bundler
/// let options = StaticOptions::new()
///     .precompressed()                   // use app.js.br / app.js.gz when the client accepts them
///     .immutable_dir("assets")           // assets/* never change under the same name
///     .spa_fallback("index.html");       // /orders/17 -> index.html for browsers
/// # let _ = options;
/// ```
#[derive(Clone)]
pub struct StaticOptions {
    cache_control: String,
    immutable_cache_control: String,
    precompressed: bool,
    spa_fallback: Option<String>,
    immutable_dirs: Vec<String>,
    hashed_names: bool,
}

impl Default for StaticOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticOptions {
    pub fn new() -> Self {
        Self {
            cache_control: "public, max-age=3600".into(),
            immutable_cache_control: "public, max-age=31536000, immutable".into(),
            precompressed: false,
            spa_fallback: None,
            immutable_dirs: Vec::new(),
            hashed_names: false,
        }
    }

    /// `Cache-Control` for ordinary files (default `public, max-age=3600`)
    pub fn cache_control(mut self, value: &str) -> Self {
        self.cache_control = value.to_string();
        self
    }

    /// Serve `file.br` or `file.gz` next to `file` when the client accepts that
    /// encoding (Brotli preferred), with `Content-Encoding` and `Vary: Accept-Encoding`.
    /// Compress at build time; nothing is compressed per request.
    pub fn precompressed(mut self) -> Self {
        self.precompressed = true;
        self
    }

    /// Answer navigations to unknown paths (`GET` accepting `text/html`, no file
    /// extension) with this file, usually `index.html`, so client-side routes
    /// survive a reload. Missing assets (`/app.js`) still give `404`.
    pub fn spa_fallback(mut self, file: &str) -> Self {
        self.spa_fallback = Some(file.trim_start_matches('/').to_string());
        self
    }

    /// Files under this directory (relative to the root) are cached for a year
    /// and marked `immutable`: use it for bundler output whose names change with content
    pub fn immutable_dir(mut self, dir: &str) -> Self {
        self.immutable_dirs.push(dir.trim_matches('/').to_string());
        self
    }

    /// Treat files whose name contains a content hash (a dot- or dash-separated
    /// segment of 8 or more hex digits, at least one a digit, as in
    /// `app.3f9a2c1b.js`) as immutable
    pub fn hashed_names(mut self) -> Self {
        self.hashed_names = true;
        self
    }

    fn is_immutable(&self, relative: &str) -> bool {
        let in_dir = self.immutable_dirs.iter().any(|d| {
            relative == d
                || relative
                    .strip_prefix(d.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        });
        in_dir || (self.hashed_names && has_hash_segment(relative))
    }
}

fn has_hash_segment(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or("");
    // the extension is not a hash
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    stem.split(['.', '-']).any(|seg| {
        seg.len() >= 8
            && seg.bytes().all(|b| b.is_ascii_hexdigit())
            && seg.bytes().any(|b| b.is_ascii_digit())
    })
}

/// Whether an `Accept-Encoding` value allows `encoding` (explicit `q=0` refuses it)
fn accepts_encoding(header: &str, encoding: &str) -> bool {
    header.split(',').any(|item| {
        let (name, params) = item.split_once(';').unwrap_or((item, ""));
        let q_zero = params
            .split(';')
            .filter_map(|p| p.trim().strip_prefix("q="))
            .any(|q| q.trim().parse::<f32>().is_ok_and(|q| q <= 0.0));
        name.trim().eq_ignore_ascii_case(encoding) && !q_zero
    })
}

/// Serves a directory: files, `index.html` for directories, ranges, conditional
/// requests and streaming for large files (see [`crate::files`]).
///
/// Dotfiles (`.env`, `.git/`, ...) are never served, and requests that try to
/// leave the directory (`..`, absolute paths, symlinks pointing outside) get `404`.
pub struct StaticFileHandler {
    root_dir: PathBuf,
    options: StaticOptions,
}

impl StaticFileHandler {
    pub fn new(root_dir: &str) -> Self {
        Self::with_options(root_dir, StaticOptions::new())
    }

    pub fn with_options(root_dir: &str, options: StaticOptions) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir),
            options,
        }
    }

    /// A browser navigation to a path that is not a file
    fn wants_fallback(&self, req: &Request, file_path: &str) -> bool {
        let last = file_path.rsplit('/').next().unwrap_or("");
        self.options.spa_fallback.is_some()
            && req.method == http::Method::GET
            && !last.contains('.')
            && req
                .headers
                .get(http::header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|a| a.contains("text/html"))
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

        self.respond(req, &canonical_root, &canonical).await
    }

    /// Serve `file` (already validated to be inside the root) with the cache
    /// policy and, if enabled, its precompressed variant
    async fn respond(&self, req: &Request, root: &Path, file: &Path) -> Result<Response> {
        let relative = file
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let cache = if self.options.is_immutable(&relative) {
            &self.options.immutable_cache_control
        } else {
            &self.options.cache_control
        };
        let mut file_options = FileOptions::new().cache_control(cache.clone());
        let mut target = file.to_path_buf();

        if self.options.precompressed {
            let accept = req
                .headers
                .get(http::header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            for (encoding, ext) in [("br", "br"), ("gzip", "gz")] {
                if !accepts_encoding(accept, encoding) {
                    continue;
                }
                let mut name = file.as_os_str().to_os_string();
                name.push(".");
                name.push(ext);
                let variant = PathBuf::from(name);
                // a symlinked variant must not leave the root either
                let inside = match fs::canonicalize(&variant).await {
                    Ok(c) => {
                        c.starts_with(root) && fs::metadata(&c).await.is_ok_and(|m| m.is_file())
                    }
                    Err(_) => false,
                };
                if inside {
                    file_options = file_options
                        .content_type(crate::files::content_type_for(file))
                        .header("content-encoding", encoding);
                    target = variant;
                    break;
                }
            }
            file_options = file_options.header("vary", "accept-encoding");
        }
        serve_file(req, &target, &file_options).await
    }
}

#[async_trait]
impl Handler for StaticFileHandler {
    async fn call(&self, req: Request) -> Result<Response> {
        let filepath = req.params.get("filepath").unwrap_or("").to_string();
        match self.serve(&req, &filepath).await {
            Err(Error::NotFound(_)) if self.wants_fallback(&req, &filepath) => {
                let index = self.options.spa_fallback.clone().unwrap_or_default();
                self.serve(&req, &index).await
            }
            other => other,
        }
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
