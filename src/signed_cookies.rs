//! Tamper-proof cookies (feature `signing`)
//!
//! A signed cookie is readable by the browser but cannot be changed without the
//! server noticing: the value is followed by an HMAC-SHA256 of the cookie name
//! and value. Use it for small, non-secret state you want to keep out of a
//! database (a theme, a feature-flag bucket, a "remember me" identifier that is
//! checked again server-side). The content is **not** encrypted, never put
//! secrets in it.
//!
//! ```
//! use ullun::signed_cookies::CookieSigner;
//!
//! let signer = CookieSigner::new(b"a secret key of at least 32 bytes!!").unwrap();
//! let signed = signer.sign("theme", "dark");
//! assert_eq!(signer.verify("theme", &signed).as_deref(), Some("dark"));
//!
//! // tampering, or moving the value to another cookie, is detected
//! assert_eq!(signer.verify("theme", &signed.replace("dark", "evil")), None);
//! assert_eq!(signer.verify("role", &signed), None);
//! ```
//!
//! In handlers use [`Response::signed_cookie`] and [`Request::signed_cookie`].
//! To rotate the key, install the new one and keep the old one for verification
//! with [`CookieSigner::previous_key`]: old cookies still verify, new ones are
//! signed with the new key.

use crate::cookies::Cookie;
use crate::error::{Error, Result};
use crate::request::Request;
use crate::response::Response;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Signs and verifies cookie values
#[derive(Clone)]
pub struct CookieSigner {
    key: Vec<u8>,
    previous: Vec<Vec<u8>>,
}

impl std::fmt::Debug for CookieSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieSigner").finish_non_exhaustive()
    }
}

impl CookieSigner {
    /// `key` must be at least 32 bytes of randomness (see
    /// [`random_bytes`](crate::security::random_bytes))
    pub fn new(key: &[u8]) -> Result<Self> {
        if key.len() < 32 {
            return Err(Error::internal(
                "Cookie signing key must be at least 32 bytes",
            ));
        }
        Ok(Self {
            key: key.to_vec(),
            previous: Vec::new(),
        })
    }

    /// An older key that still verifies cookies (key rotation)
    pub fn previous_key(mut self, key: &[u8]) -> Result<Self> {
        if key.len() < 32 {
            return Err(Error::internal(
                "Cookie signing key must be at least 32 bytes",
            ));
        }
        self.previous.push(key.to_vec());
        Ok(self)
    }

    fn mac(key: &[u8], name: &str, value: &str) -> HmacSha256 {
        // HMAC accepts keys of any length
        let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
        m.update(name.as_bytes());
        m.update(&[0]); // name and value cannot run into each other
        m.update(value.as_bytes());
        m
    }

    /// `value.signature`, safe to use as a cookie value
    pub fn sign(&self, name: &str, value: &str) -> String {
        let tag = Self::mac(&self.key, name, value).finalize().into_bytes();
        format!("{}.{}", value, URL_SAFE_NO_PAD.encode(tag))
    }

    /// The original value if `signed` carries a valid signature for cookie `name`
    pub fn verify(&self, name: &str, signed: &str) -> Option<String> {
        let (value, tag) = signed.rsplit_once('.')?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        std::iter::once(&self.key)
            .chain(&self.previous)
            .any(|key| Self::mac(key, name, value).verify_slice(&tag).is_ok())
            .then(|| value.to_string())
    }
}

impl Response {
    /// Set a cookie whose value is signed with `signer`
    pub fn signed_cookie(self, signer: &CookieSigner, cookie: Cookie) -> Self {
        let signed = signer.sign(cookie.name(), cookie.value());
        self.cookie(cookie.with_value(signed))
    }
}

impl Request {
    /// The value of a cookie set with [`Response::signed_cookie`], `None` when
    /// it is absent or its signature does not verify
    pub fn signed_cookie(&self, signer: &CookieSigner, name: &str) -> Option<String> {
        signer.verify(name, self.cookies().get(name)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn roundtrip_and_tampering() {
        let s = CookieSigner::new(KEY).unwrap();
        let signed = s.sign("c", "v.with.dots");
        assert_eq!(s.verify("c", &signed).as_deref(), Some("v.with.dots"));
        assert_eq!(s.verify("c", &signed.replace('v', "w")), None);
        assert_eq!(s.verify("other", &signed), None);
        assert_eq!(s.verify("c", "nosignature"), None);
        assert_eq!(s.verify("c", "value.!!!"), None);
        assert_eq!(s.verify("c", ""), None);
        // a different key does not verify
        let other = CookieSigner::new(b"ffffffffffffffffffffffffffffffff").unwrap();
        assert_eq!(other.verify("c", &signed), None);
    }

    #[test]
    fn name_and_value_boundaries_are_unambiguous() {
        let s = CookieSigner::new(KEY).unwrap();
        // ("ab", "c") must not verify as ("a", "bc")
        let signed = s.sign("ab", "c");
        let tag = signed.rsplit_once('.').unwrap().1;
        assert_eq!(s.verify("a", &format!("bc.{tag}")), None);
    }

    #[test]
    fn short_keys_are_refused() {
        assert!(CookieSigner::new(b"short").is_err());
        assert!(CookieSigner::new(KEY)
            .unwrap()
            .previous_key(b"short")
            .is_err());
    }

    #[test]
    fn rotation_verifies_old_cookies_and_signs_with_the_new_key() {
        let old = CookieSigner::new(KEY).unwrap();
        let old_cookie = old.sign("c", "v");
        let new_key = b"ffffffffffffffffffffffffffffffff";
        let rotated = CookieSigner::new(new_key)
            .unwrap()
            .previous_key(KEY)
            .unwrap();
        assert_eq!(rotated.verify("c", &old_cookie).as_deref(), Some("v"));
        let fresh = rotated.sign("c", "v");
        assert_ne!(fresh, old_cookie);
        // once the old key is dropped, old cookies stop verifying
        let only_new = CookieSigner::new(new_key).unwrap();
        assert_eq!(only_new.verify("c", &old_cookie), None);
        assert_eq!(only_new.verify("c", &fresh).as_deref(), Some("v"));
    }
}
