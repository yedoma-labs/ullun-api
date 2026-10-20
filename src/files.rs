//! Serving files from disk
//!
//! [`serve_file`] answers a request with a file: `Content-Type` from the
//! extension, `Last-Modified` and a weak `ETag`, conditional requests (`304`),
//! byte ranges (`206`, `416`, `If-Range`) for resumable downloads and media
//! seeking, and bodies streamed in chunks so large files never sit in memory.
//! `HEAD` works through the router.
//!
//! ```no_run
//! use ullun::files::{serve_file, FileOptions};
//! use ullun::prelude::*;
//!
//! async fn report(req: Request) -> Result<Response> {
//!     // path chosen by the server, never built from client input
//!     serve_file(&req, "/var/data/report.pdf", &FileOptions::new().attachment("report.pdf")).await
//! }
//! ```
//!
//! The caller is responsible for the path: never join client-supplied names onto
//! a directory without validating them (see `App::serve_static`, which does).

use crate::error::{Error, Result};
use crate::request::Request;
use crate::response::Response;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Bodies up to this size are read into memory in one go
const BUFFER_LIMIT: u64 = 64 * 1024;
const CHUNK: usize = 64 * 1024;

/// Per-response settings
#[derive(Debug, Clone, Default)]
pub struct FileOptions {
    cache_control: Option<String>,
    content_type: Option<String>,
    attachment: Option<String>,
}

impl FileOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Cache-Control` header value
    pub fn cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = Some(value.into());
        self
    }

    /// Override the type guessed from the extension
    pub fn content_type(mut self, value: impl Into<String>) -> Self {
        self.content_type = Some(value.into());
        self
    }

    /// Offer the file as a download under this name (`Content-Disposition: attachment`)
    pub fn attachment(mut self, filename: impl Into<String>) -> Self {
        self.attachment = Some(filename.into());
        self
    }
}

/// Serve `path`, honouring conditional and range headers of `req`
///
/// `404` if the path is not a regular file.
pub async fn serve_file(
    req: &Request,
    path: impl AsRef<Path>,
    options: &FileOptions,
) -> Result<Response> {
    let path = path.as_ref();
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::not_found("File not found"))?;
    let meta = file
        .metadata()
        .await
        .map_err(|_| Error::not_found("File not found"))?;
    if !meta.is_file() {
        return Err(Error::not_found("File not found"));
    }
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("W/\"{:x}-{:x}\"", size, mtime);

    let content_type = options
        .content_type
        .clone()
        .unwrap_or_else(|| content_type_for(path).to_string());

    let mut base = Response::ok()
        .with_header("content-type", content_type)
        .with_header("accept-ranges", "bytes")
        .with_header("etag", etag.clone())
        .with_header("last-modified", http_date(mtime));
    if let Some(cc) = &options.cache_control {
        base = base.with_header("cache-control", cc.clone());
    }
    if let Some(name) = &options.attachment {
        base = base.with_header("content-disposition", content_disposition(name));
    }

    // conditional GET: If-None-Match takes precedence over If-Modified-Since
    let header = |name: &str| req.headers.get(name).and_then(|v| v.to_str().ok());
    let not_modified = match header("if-none-match") {
        Some(inm) => crate::conditional::if_none_match_hits(inm, &etag),
        None => header("if-modified-since")
            .and_then(parse_http_date)
            .is_some_and(|since| mtime <= since),
    };
    if not_modified {
        return Ok(base.with_status(http::StatusCode::NOT_MODIFIED));
    }

    // byte range, unless If-Range says our copy differs from the client's
    let range_applies = header("if-range").is_none_or(|ir| {
        if ir.starts_with('"') || ir.starts_with("W/") {
            // a strong comparison is required, and our tag is weak
            false
        } else {
            parse_http_date(ir) == Some(mtime)
        }
    });
    let range = match header("range").filter(|_| range_applies) {
        Some(r) => parse_range(r, size),
        None => RangeOutcome::Full,
    };

    let (start, len, partial) = match range {
        RangeOutcome::Full => (0, size, false),
        RangeOutcome::Partial(start, end) => (start, end - start + 1, true),
        RangeOutcome::Unsatisfiable => {
            return Ok(
                Response::from_error(Error::custom(416, "Range not satisfiable"))
                    .with_header("content-range", format!("bytes */{}", size))
                    .with_header("accept-ranges", "bytes"),
            );
        }
    };

    let mut res = base;
    if partial {
        res = res
            .with_status(http::StatusCode::PARTIAL_CONTENT)
            .with_header(
                "content-range",
                format!("bytes {}-{}/{}", start, start + len - 1, size),
            );
    }

    if req.method == http::Method::HEAD {
        // headers only; into_head keeps this explicit length
        return Ok(res.with_header("content-length", len.to_string()));
    }

    if len <= BUFFER_LIMIT {
        let mut buf = vec![0u8; len as usize];
        if start > 0 {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|_| Error::internal("Failed to read file"))?;
        }
        file.read_exact(&mut buf)
            .await
            .map_err(|_| Error::internal("Failed to read file"))?;
        return Ok(res.with_body(buf));
    }

    let (tx, mut streamed) = Response::stream();
    streamed.status = res.status;
    streamed.headers = res.headers;
    streamed
        .headers
        .insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(len));
    tokio::spawn(async move {
        if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return;
        }
        let mut remaining = len;
        let mut buf = vec![0u8; CHUNK];
        while remaining > 0 {
            let want = (remaining as usize).min(CHUNK);
            match file.read(&mut buf[..want]).await {
                // file shrank while sending: end the body early
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    remaining -= n as u64;
                    if tx
                        .send(bytes::Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        return; // client went away
                    }
                }
            }
        }
    });
    Ok(streamed)
}

enum RangeOutcome {
    Full,
    /// Inclusive byte positions
    Partial(u64, u64),
    Unsatisfiable,
}

/// Single `bytes=` range of a representation of `size` bytes. Anything that is
/// not a single valid byte range is served in full, as RFC 9110 allows.
fn parse_range(header: &str, size: u64) -> RangeOutcome {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Full;
    };
    if spec.contains(',') {
        return RangeOutcome::Full;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeOutcome::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    match (first.is_empty(), last.is_empty()) {
        // suffix: the last N bytes
        (true, false) => match last.parse::<u64>() {
            Ok(0) => RangeOutcome::Unsatisfiable,
            Ok(_) if size == 0 => RangeOutcome::Unsatisfiable,
            Ok(n) => RangeOutcome::Partial(size.saturating_sub(n), size - 1),
            Err(_) => RangeOutcome::Full,
        },
        (false, empty_last) => {
            let Ok(start) = first.parse::<u64>() else {
                return RangeOutcome::Full;
            };
            let end = if empty_last {
                u64::MAX
            } else {
                match last.parse::<u64>() {
                    Ok(e) if e >= start => e,
                    _ => return RangeOutcome::Full, // last < first is invalid syntax
                }
            };
            if start >= size {
                RangeOutcome::Unsatisfiable
            } else {
                RangeOutcome::Partial(start, end.min(size - 1))
            }
        }
        (true, true) => RangeOutcome::Full,
    }
}

/// `attachment; filename="fallback.txt"; filename*=UTF-8''percent-encoded`
fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        fallback,
        urlencoding::encode(name)
    )
}

/// `Content-Type` for a path, by extension (`application/octet-stream` if unknown)
pub fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("xml") => "application/xml",
        Some("txt" | "log") => "text/plain; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("wasm") => "application/wasm",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "oga") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// HTTP date (IMF-fixdate), e.g. `Sun, 06 Nov 1994 08:49:37 GMT`
pub fn http_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[(days.rem_euclid(7)) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Parse an IMF-fixdate (the only format servers must generate)
pub fn parse_http_date(s: &str) -> Option<u64> {
    let mut parts = s.trim().split(' ');
    let day_name = parts.next()?;
    if !day_name.ends_with(',') {
        return None;
    }
    let day: i64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS.iter().position(|m| *m == month_name)? as i64 + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let (h, m, sec): (u64, u64, u64) = (
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
    );
    if parts.next()? != "GMT" || parts.next().is_some() || hms.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 || !(1970..=9999).contains(&year) {
        return None;
    }
    // days-from-civil (Howard Hinnant)
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days)
        .ok()
        .map(|d| d * 86_400 + h * 3600 + m * 60 + sec)
}

/// Current time as an HTTP date
pub fn http_date_now() -> String {
    http_date(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates_roundtrip_and_match_the_rfc_example() {
        // RFC 9110 section 5.6.7
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT"); // leap day
        for secs in [
            0u64,
            1,
            86_399,
            86_400,
            951_782_400,
            1_700_000_000,
            4_102_444_799,
            253_402_300_799,
        ] {
            assert_eq!(parse_http_date(&http_date(secs)), Some(secs), "{secs}");
        }
    }

    #[test]
    fn bad_http_dates_are_rejected() {
        for bad in [
            "",
            "yesterday",
            "Sun, 06 Nov 1994 08:49:37",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 25:49:37 GMT",
            "Sun, 06 Nov 1969 00:00:00 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad:?}");
        }
    }

    fn range(h: &str, size: u64) -> Option<(u64, u64)> {
        match parse_range(h, size) {
            RangeOutcome::Partial(a, b) => Some((a, b)),
            _ => None,
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(range("bytes=-5000", 1000), Some((0, 999)));
        assert_eq!(range("bytes=990-5000", 1000), Some((990, 999)));
        assert_eq!(range("bytes=0-0", 1000), Some((0, 0)));
        assert_eq!(range(" bytes= 5 - 9 ", 1000), Some((5, 9)));
        // unsatisfiable
        assert!(matches!(
            parse_range("bytes=1000-", 1000),
            RangeOutcome::Unsatisfiable
        ));
        assert!(matches!(
            parse_range("bytes=2000-3000", 1000),
            RangeOutcome::Unsatisfiable
        ));
        assert!(matches!(
            parse_range("bytes=-0", 1000),
            RangeOutcome::Unsatisfiable
        ));
        assert!(matches!(
            parse_range("bytes=0-0", 0),
            RangeOutcome::Unsatisfiable
        ));
        assert!(matches!(
            parse_range("bytes=-5", 0),
            RangeOutcome::Unsatisfiable
        ));
        // not a single valid range: serve everything
        for ignored in [
            "bytes=0-1,5-6",
            "bytes=abc",
            "bytes=5-2",
            "items=0-1",
            "0-1",
            "bytes=",
            "bytes=-",
            "bytes=a-b",
        ] {
            assert!(
                matches!(parse_range(ignored, 1000), RangeOutcome::Full),
                "{ignored}"
            );
        }
        // overflow must not panic
        assert!(range("bytes=0-99999999999999999999", 10).is_none());
        assert_eq!(range(&format!("bytes=0-{}", u64::MAX), 10), Some((0, 9)));
    }

    #[test]
    fn attachment_header_has_ascii_fallback_and_utf8_name() {
        assert_eq!(
            content_disposition("report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
        let h = content_disposition("Übersicht \"Q1\".pdf");
        assert!(h.contains("filename=\"_bersicht _Q1_.pdf\""), "{h}");
        assert!(
            h.contains("filename*=UTF-8''%C3%9Cbersicht%20%22Q1%22.pdf"),
            "{h}"
        );
        assert!(!content_disposition("a\r\nb").contains('\r'));
    }

    #[test]
    fn content_types() {
        let t = |p: &str| content_type_for(Path::new(p));
        assert_eq!(t("a.HTML"), "text/html; charset=utf-8");
        assert_eq!(t("photo.JPeG"), "image/jpeg");
        assert_eq!(t("video.mp4"), "video/mp4");
        assert_eq!(t("noext"), "application/octet-stream");
        assert_eq!(t("archive.tar.gz"), "application/gzip");
    }
}
