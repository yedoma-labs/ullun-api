//! Encrypted cookies (feature `encryption`)
//!
//! A private cookie is unreadable and tamper-proof: the value is encrypted and
//! authenticated with XChaCha20-Poly1305, and the cookie name is bound to the
//! ciphertext so a value cannot be moved to another cookie. Use it for state
//! the browser must not see (a user id, a flash message). Prefer a server-side
//! [`session`](crate::session) for anything large: cookies are limited to about
//! 4 KB.
//!
//! ```
//! use ullun::private_cookies::CookieCipher;
//!
//! let cipher = CookieCipher::new(b"a secret key of exactly 32 bytes").unwrap();
//! let sealed = cipher.seal("flash", "saved!").unwrap();
//! assert!(!sealed.contains("saved"));
//! assert_eq!(cipher.open("flash", &sealed).as_deref(), Some("saved!"));
//!
//! // another cookie name, or a changed value, does not open
//! assert_eq!(cipher.open("role", &sealed), None);
//! ```
//!
//! In handlers use [`Response::private_cookie`] and [`Request::private_cookie`].
//! To rotate the key, install the new one and keep the old one for opening with
//! [`CookieCipher::previous_key`].

use crate::cookies::Cookie;
use crate::error::{Error, Result};
use crate::request::Request;
use crate::response::Response;
use crate::security::random_bytes;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

fn cipher_for(key: &[u8]) -> Result<XChaCha20Poly1305> {
    if key.len() != KEY_LEN {
        return Err(Error::internal(
            "Cookie encryption key must be exactly 32 bytes",
        ));
    }
    XChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| Error::internal("Cookie encryption key must be exactly 32 bytes"))
}

/// Encrypts and decrypts cookie values
#[derive(Clone)]
pub struct CookieCipher {
    key: XChaCha20Poly1305,
    previous: Vec<XChaCha20Poly1305>,
}

impl std::fmt::Debug for CookieCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieCipher").finish_non_exhaustive()
    }
}

impl CookieCipher {
    /// `key` must be exactly 32 bytes of randomness (see
    /// [`random_bytes`](crate::security::random_bytes))
    pub fn new(key: &[u8]) -> Result<Self> {
        Ok(Self {
            key: cipher_for(key)?,
            previous: Vec::new(),
        })
    }

    /// An older key that still opens cookies (key rotation)
    pub fn previous_key(mut self, key: &[u8]) -> Result<Self> {
        self.previous.push(cipher_for(key)?);
        Ok(self)
    }

    /// Encrypt `value` for cookie `name`; the result is safe to use as a cookie value
    pub fn seal(&self, name: &str, value: &str) -> Result<String> {
        let nonce_bytes = random_bytes(NONCE_LEN)?;
        let nonce = XNonce::try_from(nonce_bytes.as_slice())
            .map_err(|_| Error::internal("Invalid cookie nonce"))?;
        let ciphertext = self
            .key
            .encrypt(
                &nonce,
                Payload {
                    msg: value.as_bytes(),
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| Error::internal("Cookie encryption failed"))?;
        let mut out = nonce_bytes;
        out.extend_from_slice(&ciphertext);
        Ok(URL_SAFE_NO_PAD.encode(out))
    }

    /// The original value if `sealed` was produced by [`seal`](Self::seal) for
    /// cookie `name` and was not modified
    pub fn open(&self, name: &str, sealed: &str) -> Option<String> {
        let raw = URL_SAFE_NO_PAD.decode(sealed).ok()?;
        if raw.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ciphertext) = raw.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce).ok()?;
        std::iter::once(&self.key)
            .chain(&self.previous)
            .find_map(|key| {
                key.decrypt(
                    &nonce,
                    Payload {
                        msg: ciphertext,
                        aad: name.as_bytes(),
                    },
                )
                .ok()
            })
            .and_then(|plain| String::from_utf8(plain).ok())
    }
}

impl Response {
    /// Set a cookie whose value is encrypted with `cipher`
    pub fn private_cookie(self, cipher: &CookieCipher, cookie: Cookie) -> Result<Self> {
        let sealed = cipher.seal(cookie.name(), cookie.value())?;
        Ok(self.cookie(cookie.with_value(sealed)))
    }
}

impl Request {
    /// The value of a cookie set with [`Response::private_cookie`], `None` when
    /// it is absent or does not decrypt
    pub fn private_cookie(&self, cipher: &CookieCipher, name: &str) -> Option<String> {
        cipher.open(name, self.cookies().get(name)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    const OTHER: &[u8] = b"ffffffffffffffffffffffffffffffff";

    #[test]
    fn roundtrip_hides_the_value() {
        let c = CookieCipher::new(KEY).unwrap();
        let sealed = c.seal("c", "secret value").unwrap();
        assert!(!sealed.contains("secret"));
        assert_eq!(c.open("c", &sealed).as_deref(), Some("secret value"));
        assert_eq!(c.open("c", &c.seal("c", "").unwrap()).as_deref(), Some(""));
    }

    #[test]
    fn sealing_twice_gives_different_ciphertexts() {
        let c = CookieCipher::new(KEY).unwrap();
        assert_ne!(c.seal("c", "v").unwrap(), c.seal("c", "v").unwrap());
    }

    #[test]
    fn tampering_and_wrong_name_or_key_are_rejected() {
        let c = CookieCipher::new(KEY).unwrap();
        let sealed = c.seal("c", "v").unwrap();
        assert_eq!(c.open("other", &sealed), None);
        assert_eq!(CookieCipher::new(OTHER).unwrap().open("c", &sealed), None);
        let mut raw = URL_SAFE_NO_PAD.decode(&sealed).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        assert_eq!(c.open("c", &URL_SAFE_NO_PAD.encode(raw)), None);
        assert_eq!(c.open("c", "!!!"), None);
        assert_eq!(c.open("c", "short"), None);
        assert_eq!(c.open("c", ""), None);
    }

    #[test]
    fn keys_must_be_32_bytes() {
        assert!(CookieCipher::new(b"short").is_err());
        assert!(CookieCipher::new(&[0u8; 33]).is_err());
        assert!(CookieCipher::new(KEY).unwrap().previous_key(b"x").is_err());
    }

    #[test]
    fn rotation_opens_old_cookies_and_seals_with_the_new_key() {
        let old = CookieCipher::new(KEY).unwrap().seal("c", "v").unwrap();
        let rotated = CookieCipher::new(OTHER).unwrap().previous_key(KEY).unwrap();
        assert_eq!(rotated.open("c", &old).as_deref(), Some("v"));
        let fresh = rotated.seal("c", "v").unwrap();
        let only_new = CookieCipher::new(OTHER).unwrap();
        assert_eq!(only_new.open("c", &old), None);
        assert_eq!(only_new.open("c", &fresh).as_deref(), Some("v"));
    }
}
