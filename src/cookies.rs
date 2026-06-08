//! Cookie parsing and management

use std::collections::HashMap;
use std::fmt;

/// A single HTTP cookie
#[derive(Debug, Clone)]
pub struct Cookie {
    name: String,
    value: String,
    domain: Option<String>,
    path: Option<String>,
    max_age: Option<i64>,
    secure: bool,
    http_only: bool,
    same_site: Option<SameSite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl Cookie {
    /// Create a new cookie
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        let value = value.into();
        
        // Validate cookie name and value to prevent injection
        if name.contains(|c: char| c == ';' || c == '=' || c.is_control()) {
            panic!("Invalid cookie name: contains forbidden characters");
        }
        if value.contains(|c: char| c == ';' || c.is_control()) {
            panic!("Invalid cookie value: contains forbidden characters");
        }
        
        Self {
            name,
            value,
            domain: None,
            path: Some("/".to_string()),
            max_age: None,
            secure: false,
            http_only: false,
            same_site: None,
        }
    }

    /// Set the domain
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set the path
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set max-age in seconds
    pub fn max_age(mut self, seconds: i64) -> Self {
        self.max_age = Some(seconds);
        self
    }

    /// Mark as secure (HTTPS only)
    pub fn secure(mut self) -> Self {
        self.secure = true;
        self
    }

    /// Mark as HttpOnly (not accessible via JavaScript)
    pub fn http_only(mut self) -> Self {
        self.http_only = true;
        self
    }

    /// Set SameSite attribute
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        // SameSite=None requires Secure flag per RFC 6265bis
        if same_site == SameSite::None {
            self.secure = true;
        }
        self.same_site = Some(same_site);
        self
    }

    /// Get cookie name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get cookie value
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Convert to Set-Cookie header value
    pub fn to_header_value(&self) -> String {
        let mut parts = vec![format!("{}={}", self.name, self.value)];

        if let Some(domain) = &self.domain {
            parts.push(format!("Domain={}", domain));
        }

        if let Some(path) = &self.path {
            parts.push(format!("Path={}", path));
        }

        if let Some(max_age) = self.max_age {
            parts.push(format!("Max-Age={}", max_age));
        }

        if self.secure {
            parts.push("Secure".to_string());
        }

        if self.http_only {
            parts.push("HttpOnly".to_string());
        }

        if let Some(same_site) = self.same_site {
            parts.push(format!("SameSite={}", same_site));
        }

        parts.join("; ")
    }
}

impl fmt::Display for SameSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SameSite::Strict => write!(f, "Strict"),
            SameSite::Lax => write!(f, "Lax"),
            SameSite::None => write!(f, "None"),
        }
    }
}

/// Collection of cookies from a request
#[derive(Debug, Clone, Default)]
pub struct Cookies {
    cookies: HashMap<String, String>,
}

impl Cookies {
    /// Create empty cookies collection
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse cookies from Cookie header value
    pub fn parse(cookie_header: &str) -> Self {
        let mut cookies = HashMap::new();

        for pair in cookie_header.split(';') {
            let pair = pair.trim();
            if let Some((name, value)) = pair.split_once('=') {
                cookies.insert(name.trim().to_string(), value.trim().to_string());
            }
        }

        Self { cookies }
    }

    /// Get a cookie value by name
    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(|s| s.as_str())
    }

    /// Check if a cookie exists
    pub fn contains(&self, name: &str) -> bool {
        self.cookies.contains_key(name)
    }

    /// Get all cookie names
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.cookies.keys().map(|s| s.as_str())
    }

    /// Get the number of cookies
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Check if there are no cookies
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cookie_basic() {
        let cookie = Cookie::new("session", "abc123");
        assert_eq!(cookie.name(), "session");
        assert_eq!(cookie.value(), "abc123");
    }

    #[test]
    fn test_cookie_to_header() {
        let cookie = Cookie::new("session", "abc123")
            .path("/")
            .max_age(3600)
            .http_only()
            .secure()
            .same_site(SameSite::Strict);

        let header = cookie.to_header_value();
        assert!(header.contains("session=abc123"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("Max-Age=3600"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Secure"));
        assert!(header.contains("SameSite=Strict"));
    }

    #[test]
    fn test_cookies_parse() {
        let cookies = Cookies::parse("session=abc123; user=john; theme=dark");
        assert_eq!(cookies.get("session"), Some("abc123"));
        assert_eq!(cookies.get("user"), Some("john"));
        assert_eq!(cookies.get("theme"), Some("dark"));
        assert_eq!(cookies.len(), 3);
    }

    #[test]
    fn test_cookies_parse_with_spaces() {
        let cookies = Cookies::parse("session=abc123 ; user = john  ;  theme=dark");
        assert_eq!(cookies.get("session"), Some("abc123"));
        assert_eq!(cookies.get("user"), Some("john"));
        assert_eq!(cookies.get("theme"), Some("dark"));
    }

    #[test]
    fn test_cookies_empty() {
        let cookies = Cookies::new();
        assert!(cookies.is_empty());
        assert_eq!(cookies.len(), 0);
    }
}
