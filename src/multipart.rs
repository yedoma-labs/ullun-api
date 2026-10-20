//! `multipart/form-data` request bodies (file uploads and mixed forms)
//!
//! ```
//! use ullun::prelude::*;
//!
//! async fn upload(req: Request) -> Result<Response> {
//!     let form = req.multipart()?;
//!     let title = form.text("title").unwrap_or("untitled");
//!     let Some(file) = form.file("avatar") else {
//!         return Err(Error::bad_request("avatar is required"));
//!     };
//!     // never use the client's file name as a path: sanitize it, and store
//!     // the bytes under a name you generated
//!     let name = file.safe_filename().unwrap_or_else(|| "upload".into());
//!     Ok(Response::json(serde_json::json!({ "title": title, "name": name, "bytes": file.data.len() })))
//! }
//! ```
//!
//! The whole body is buffered, so it is bounded by the request body limit:
//! raise it for upload routes with `App::body_limit` / `RouteGroup::max_body_size`
//! and keep it modest. Parsing itself is limited by [`MultipartLimits`].

use crate::error::{Error, Result};
use crate::request::Request;
use bytes::Bytes;

/// Bounds against hostile bodies
#[derive(Debug, Clone)]
pub struct MultipartLimits {
    /// Parts accepted in one body (default 64)
    pub max_parts: usize,
    /// Bytes of headers per part (default 8 KiB)
    pub max_header_bytes: usize,
}

impl Default for MultipartLimits {
    fn default() -> Self {
        Self {
            max_parts: 64,
            max_header_bytes: 8 * 1024,
        }
    }
}

/// One field or file of the form
#[derive(Debug, Clone)]
pub struct Part {
    /// The form field name
    pub name: String,
    /// Set for file inputs; chosen by the client, so untrusted
    pub filename: Option<String>,
    /// The part's `Content-Type`, if sent; chosen by the client, so untrusted
    pub content_type: Option<String>,
    pub data: Bytes,
}

impl Part {
    /// The data as text, if it is valid UTF-8
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.data).ok()
    }

    pub fn is_file(&self) -> bool {
        self.filename.is_some()
    }

    /// The client's file name reduced to something safe to display or use as a
    /// download name: no directories, no control or special characters, no
    /// leading dots, at most 120 characters. `None` if nothing is left.
    ///
    /// Do not use it to build paths for storing the upload: generate your own name.
    pub fn safe_filename(&self) -> Option<String> {
        sanitize_filename(self.filename.as_deref()?)
    }
}

/// See [`Part::safe_filename`]
pub fn sanitize_filename(raw: &str) -> Option<String> {
    // the last path component, whichever separator the client's OS uses
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '"' | '<' | '>' | ':' | '|' | '?' | '*' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let cleaned: String = cleaned.chars().take(120).collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']).to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// The parts of a `multipart/form-data` body, in order
#[derive(Debug, Clone, Default)]
pub struct Multipart {
    parts: Vec<Part>,
}

impl Multipart {
    pub fn iter(&self) -> std::slice::Iter<'_, Part> {
        self.parts.iter()
    }

    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// First part with this field name
    pub fn field(&self, name: &str) -> Option<&Part> {
        self.parts.iter().find(|p| p.name == name)
    }

    /// First non-file field as text
    pub fn text(&self, name: &str) -> Option<&str> {
        self.parts
            .iter()
            .find(|p| p.name == name && !p.is_file())
            .and_then(Part::text)
    }

    /// First file with this field name
    pub fn file(&self, name: &str) -> Option<&Part> {
        self.parts.iter().find(|p| p.name == name && p.is_file())
    }

    /// All files, in order (for `<input type=file multiple>`)
    pub fn files(&self) -> impl Iterator<Item = &Part> {
        self.parts.iter().filter(|p| p.is_file())
    }

    /// Parse `body` with the given boundary
    pub fn parse(body: &Bytes, boundary: &str, limits: &MultipartLimits) -> Result<Self> {
        parse(body, boundary, limits)
    }
}

impl<'a> IntoIterator for &'a Multipart {
    type Item = &'a Part;
    type IntoIter = std::slice::Iter<'a, Part>;
    fn into_iter(self) -> Self::IntoIter {
        self.parts.iter()
    }
}

impl Request {
    /// Parse a `multipart/form-data` body with the default [`MultipartLimits`]
    ///
    /// `415` for other content types, `400` for malformed bodies.
    pub fn multipart(&self) -> Result<Multipart> {
        self.multipart_with(&MultipartLimits::default())
    }

    pub fn multipart_with(&self, limits: &MultipartLimits) -> Result<Multipart> {
        let content_type = self
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let boundary = boundary_of(content_type)?;
        Multipart::parse(&self.body, &boundary, limits)
    }
}

fn boundary_of(content_type: &str) -> Result<String> {
    let mut params = content_type.split(';');
    let essence = params.next().unwrap_or("").trim();
    if !essence.eq_ignore_ascii_case("multipart/form-data") {
        return Err(Error::custom(
            415,
            "Expected content-type multipart/form-data",
        ));
    }
    for p in params {
        let Some((key, value)) = p.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim().trim_matches('"');
            let valid = !value.is_empty()
                && value.len() <= 70
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"'()+_,-./:=? ".contains(&b));
            return if valid {
                Ok(value.to_string())
            } else {
                Err(Error::bad_request("Invalid multipart boundary"))
            };
        }
    }
    Err(Error::bad_request("Missing multipart boundary"))
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn malformed(why: &str) -> Error {
    Error::bad_request(format!("Malformed multipart body: {}", why))
}

fn parse(body: &Bytes, boundary: &str, limits: &MultipartLimits) -> Result<Multipart> {
    let dash = format!("--{}", boundary).into_bytes();
    // every later delimiter is preceded by the CRLF that ends the previous part
    let delim = [b"\r\n".as_slice(), &dash].concat();

    // preamble (ignored), then the first delimiter at the start or after a CRLF
    let mut pos = if body.starts_with(&dash) {
        dash.len()
    } else {
        find(body, &delim, 0).ok_or_else(|| malformed("no boundary found"))? + delim.len()
    };

    let mut parts = Vec::new();
    loop {
        // after a delimiter: `--` ends the body, otherwise (transport padding and) CRLF
        if body[pos..].starts_with(b"--") {
            return Ok(Multipart { parts });
        }
        while matches!(body.get(pos), Some(b' ' | b'\t')) {
            pos += 1;
        }
        if !body[pos..].starts_with(b"\r\n") {
            return Err(malformed("expected line break after boundary"));
        }
        pos += 2;

        if parts.len() >= limits.max_parts {
            return Err(Error::custom(
                413,
                format!("Too many multipart parts (max {})", limits.max_parts),
            ));
        }

        // headers end at the first empty line (a part without headers starts with CRLF)
        let (headers_end, data_start) = if body[pos..].starts_with(b"\r\n") {
            (pos, pos + 2)
        } else {
            let end = find(body, b"\r\n\r\n", pos)
                .ok_or_else(|| malformed("unterminated part headers"))?;
            (end, end + 4)
        };
        if headers_end - pos > limits.max_header_bytes {
            return Err(Error::custom(
                413,
                format!(
                    "Multipart part headers larger than {} bytes",
                    limits.max_header_bytes
                ),
            ));
        }
        let header_text = std::str::from_utf8(&body[pos..headers_end])
            .map_err(|_| malformed("part headers are not UTF-8"))?;

        let data_end =
            find(body, &delim, data_start).ok_or_else(|| malformed("unterminated part"))?;
        let part = build_part(header_text, body.slice(data_start..data_end))?;
        parts.push(part);
        pos = data_end + delim.len();
    }
}

fn build_part(header_text: &str, data: Bytes) -> Result<Part> {
    let mut disposition = None;
    let mut content_type = None;
    for line in header_text.split("\r\n").filter(|l| !l.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed("bad part header"))?;
        match name.trim().to_ascii_lowercase().as_str() {
            "content-disposition" => disposition = Some(value.trim().to_string()),
            "content-type" => content_type = Some(value.trim().to_string()),
            _ => {}
        }
    }
    let disposition = disposition.ok_or_else(|| malformed("part without content-disposition"))?;
    let mut tokens = split_params(&disposition).into_iter();
    if !tokens
        .next()
        .is_some_and(|(k, v)| v.is_none() && k.eq_ignore_ascii_case("form-data"))
    {
        return Err(malformed("part is not form-data"));
    }
    let (mut name, mut filename, mut filename_ext) = (None, None, None);
    for (key, value) in tokens {
        let Some(value) = value else { continue };
        match key.to_ascii_lowercase().as_str() {
            "name" => name = Some(value),
            "filename" => filename = Some(value),
            "filename*" => filename_ext = decode_ext_value(&value),
            _ => {}
        }
    }
    Ok(Part {
        name: name.ok_or_else(|| malformed("part without a name"))?,
        // RFC 6266: the extended form wins when both are present
        filename: filename_ext.or(filename),
        content_type,
        data,
    })
}

/// `form-data; name="a b"; filename="x;y.txt"` -> [("form-data", None), ("name", Some("a b")), ...]
fn split_params(s: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ';') {
            chars.next();
        }
        if chars.peek().is_none() {
            return out;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c == ';' {
                break;
            }
            key.push(c);
            chars.next();
        }
        let key = key.trim().to_string();
        if chars.peek() != Some(&'=') {
            out.push((key, None));
            continue;
        }
        chars.next(); // '='
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ';' {
                    break;
                }
                value.push(c);
                chars.next();
            }
            value = value.trim().to_string();
        }
        out.push((key, Some(value)));
    }
}

/// RFC 5987 `UTF-8''%E2%82%AC%20rates.txt`
fn decode_ext_value(value: &str) -> Option<String> {
    let rest = value
        .strip_prefix("UTF-8''")
        .or_else(|| value.strip_prefix("utf-8''"))?;
    urlencoding::decode(rest).ok().map(|s| s.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(boundary: &str, parts: &[(&str, &[u8])]) -> Bytes {
        let mut out = Vec::new();
        for (headers, data) in parts {
            out.extend_from_slice(format!("--{boundary}\r\n{headers}\r\n\r\n").as_bytes());
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Bytes::from(out)
    }

    fn parse_ok(b: &Bytes, boundary: &str) -> Multipart {
        parse(b, boundary, &MultipartLimits::default()).unwrap()
    }

    #[test]
    fn fields_and_files_in_order() {
        let b = body(
            "XyZ",
            &[
                ("Content-Disposition: form-data; name=\"title\"", b"Hello \xC3\xA4"),
                (
                    "Content-Disposition: form-data; name=\"doc\"; filename=\"a.txt\"\r\nContent-Type: text/plain",
                    b"line1\r\nline2",
                ),
                ("Content-Disposition: form-data; name=\"doc\"; filename=\"b.bin\"", &[0, 255, 13, 10, 0]),
            ],
        );
        let form = parse_ok(&b, "XyZ");
        assert_eq!(form.len(), 3);
        assert_eq!(form.text("title"), Some("Hello ä"));
        let a = form.file("doc").unwrap();
        assert_eq!(
            (a.filename.as_deref(), a.content_type.as_deref()),
            (Some("a.txt"), Some("text/plain"))
        );
        assert_eq!(&a.data[..], b"line1\r\nline2");
        let files: Vec<_> = form.files().map(|f| f.filename.clone().unwrap()).collect();
        assert_eq!(files, ["a.txt", "b.bin"]);
        // binary content with CRLF and NUL survives
        assert_eq!(&form.files().nth(1).unwrap().data[..], &[0, 255, 13, 10, 0]);
        // a file is not a text field
        assert_eq!(form.text("doc"), None);
        assert!(form.field("missing").is_none());
    }

    #[test]
    fn preamble_epilogue_and_empty_parts() {
        let mut raw = b"ignored preamble\r\n".to_vec();
        raw.extend_from_slice(&body(
            "b",
            &[("Content-Disposition: form-data; name=\"empty\"", b"")],
        ));
        raw.extend_from_slice(b"epilogue that is ignored");
        let form = parse_ok(&Bytes::from(raw), "b");
        assert_eq!(form.len(), 1);
        assert_eq!(form.text("empty"), Some(""));

        // an empty form is valid
        assert!(parse_ok(&Bytes::from_static(b"--b--\r\n"), "b").is_empty());
    }

    #[test]
    fn data_that_looks_like_a_boundary_is_kept_when_not_a_full_delimiter() {
        let tricky = b"--b not a delimiter because no CRLF before it, and --b-- too";
        let b = body(
            "b",
            &[("Content-Disposition: form-data; name=\"x\"", tricky)],
        );
        assert_eq!(parse_ok(&b, "b").field("x").unwrap().data.as_ref(), tricky);
    }

    #[test]
    fn quoted_params_escapes_and_extended_filenames() {
        let b = body(
            "b",
            &[
                ("Content-Disposition: form-data; name=\"we;ird \\\"name\\\"\"; filename=\"x;y.txt\"", b"1"),
                (
                    "Content-Disposition: form-data; name=\"f\"; filename=\"fallback.txt\"; filename*=UTF-8''%E2%82%AC%20rates.txt",
                    b"2",
                ),
                ("content-disposition: FORM-DATA; NAME=unquoted", b"3"),
            ],
        );
        let form = parse_ok(&b, "b");
        let parts: Vec<_> = form.iter().collect();
        assert_eq!(parts[0].name, "we;ird \"name\"");
        assert_eq!(parts[0].filename.as_deref(), Some("x;y.txt"));
        assert_eq!(parts[1].filename.as_deref(), Some("€ rates.txt"));
        assert_eq!(parts[2].name, "unquoted");
    }

    #[test]
    fn malformed_bodies_are_400() {
        let l = MultipartLimits::default();
        let cases: Vec<(&str, Bytes)> = vec![
            ("no boundary", Bytes::from_static(b"just text")),
            (
                "no end",
                Bytes::from_static(
                    b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\ndata",
                ),
            ),
            (
                "headers unterminated",
                Bytes::from_static(b"--b\r\nContent-Disposition: form-data; name=\"a\""),
            ),
            ("garbage after boundary", Bytes::from_static(b"--bX\r\n")),
            ("no disposition", body("b", &[("X-Other: 1", b"d")])),
            (
                "not form-data",
                body(
                    "b",
                    &[("Content-Disposition: attachment; name=\"a\"", b"d")],
                ),
            ),
            (
                "no name",
                body("b", &[("Content-Disposition: form-data", b"d")]),
            ),
            (
                "bad header line",
                body("b", &[("Content-Disposition form-data", b"d")]),
            ),
        ];
        for (why, b) in cases {
            let err = parse(&b, "b", &l).unwrap_err();
            assert_eq!(err.status_code(), 400, "{why}: {}", err.message());
        }
    }

    #[test]
    fn limits_answer_413() {
        let many: Vec<(&str, &[u8])> = (0..5)
            .map(|_| {
                (
                    "Content-Disposition: form-data; name=\"a\"",
                    b"x".as_slice(),
                )
            })
            .collect();
        let b = body("b", &many);
        let few = MultipartLimits {
            max_parts: 4,
            ..Default::default()
        };
        assert_eq!(parse(&b, "b", &few).unwrap_err().status_code(), 413);
        assert!(parse(
            &b,
            "b",
            &MultipartLimits {
                max_parts: 5,
                ..Default::default()
            }
        )
        .is_ok());

        let long = format!(
            "Content-Disposition: form-data; name=\"a\"\r\nX-Pad: {}",
            "p".repeat(300)
        );
        let b = body("b", &[(long.as_str(), b"x")]);
        let tight = MultipartLimits {
            max_header_bytes: 100,
            ..Default::default()
        };
        assert_eq!(parse(&b, "b", &tight).unwrap_err().status_code(), 413);
    }

    #[test]
    fn boundary_from_content_type() {
        assert_eq!(
            boundary_of("multipart/form-data; boundary=abc").unwrap(),
            "abc"
        );
        assert_eq!(
            boundary_of("Multipart/Form-Data; charset=utf-8; BOUNDARY=\"a b\"").unwrap(),
            "a b"
        );
        assert_eq!(
            boundary_of("application/json").unwrap_err().status_code(),
            415
        );
        assert_eq!(boundary_of("").unwrap_err().status_code(), 415);
        assert_eq!(
            boundary_of("multipart/form-data")
                .unwrap_err()
                .status_code(),
            400
        );
        assert_eq!(
            boundary_of("multipart/form-data; boundary=")
                .unwrap_err()
                .status_code(),
            400
        );
        assert_eq!(
            boundary_of(&format!("multipart/form-data; boundary={}", "x".repeat(71)))
                .unwrap_err()
                .status_code(),
            400
        );
        assert_eq!(
            boundary_of("multipart/form-data; boundary=a\"b<c")
                .unwrap_err()
                .status_code(),
            400
        );
    }

    #[test]
    fn filenames_are_sanitized() {
        let s = |x: &str| sanitize_filename(x);
        assert_eq!(s("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(s("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(s("C:\\Users\\me\\evil.exe").as_deref(), Some("evil.exe"));
        assert_eq!(s("..hidden").as_deref(), Some("hidden"));
        assert_eq!(s(".htaccess").as_deref(), Some("htaccess"));
        assert_eq!(
            s("a<b>:c|d?e*\"f.txt").as_deref(),
            Some("a_b__c_d_e__f.txt")
        );
        assert_eq!(s("tab\there\nnl.txt").as_deref(), Some("tab_here_nl.txt"));
        assert_eq!(s("ünïcödé 日本.txt").as_deref(), Some("ünïcödé 日本.txt"));
        assert_eq!(s("name. . ").as_deref(), Some("name"));
        assert_eq!(s(&"a".repeat(300)).unwrap().len(), 120);
        for empty in ["", "   ", "..", "/", "a/", "../", ". . ."] {
            assert_eq!(s(empty), None, "{empty:?}");
        }
    }

    #[test]
    fn request_helper_checks_the_content_type() {
        let b = body(
            "zz",
            &[("Content-Disposition: form-data; name=\"a\"", b"1")],
        );
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            "multipart/form-data; boundary=zz".parse().unwrap(),
        );
        let req = Request::new(http::Method::POST, "/".parse().unwrap(), headers, b.clone());
        assert_eq!(req.multipart().unwrap().text("a"), Some("1"));

        let plain = Request::new(
            http::Method::POST,
            "/".parse().unwrap(),
            http::HeaderMap::new(),
            b,
        );
        assert_eq!(plain.multipart().unwrap_err().status_code(), 415);
    }
}
