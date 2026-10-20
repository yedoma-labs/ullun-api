//! Small security helpers: secure random tokens and constant-time comparison
//!
//! ```
//! use ullun::security::{constant_time_eq, random_token};
//!
//! let token = random_token(16).unwrap();          // 128 bits as 32 hex characters
//! assert_eq!(token.len(), 32);
//! assert!(constant_time_eq(token.as_bytes(), token.as_bytes()));
//! ```

use crate::error::{Error, Result};

/// Compare two byte strings without returning early on the first difference,
/// so the time taken does not reveal how much of a secret was guessed right.
/// Different lengths compare unequal (the length itself is not hidden).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    std::hint::black_box(diff) == 0
}

/// `len` bytes from the operating system's CSPRNG
pub fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf)
        .map_err(|e| Error::internal(format!("Random number generator unavailable: {}", e)))?;
    Ok(buf)
}

/// Unguessable token: `bytes` random bytes as lowercase hex (`2 * bytes`
/// characters). Use at least 16 bytes for session ids and secrets.
pub fn random_token(bytes: usize) -> Result<String> {
    let mut out = String::with_capacity(bytes * 2);
    for b in random_bytes(bytes)? {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0xf), 16).unwrap_or('0'));
    }
    Ok(out)
}

/// Random string of `len` characters from `[A-Za-z0-9]`, uniformly distributed
/// (rejection sampling). Handy for short public ids; for secrets prefer
/// [`random_token`].
pub fn random_alphanumeric(len: usize) -> Result<String> {
    const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in random_bytes(len.max(8))? {
            // 248 = 4 * 62: values above would skew the distribution
            if b < 248 && out.len() < len {
                out.push(ALPHABET[usize::from(b) % 62] as char);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn constant_time_eq_semantics() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        // a differing length must not be masked by a matching prefix
        assert!(!constant_time_eq(b"abc", b"abc\0"));
    }

    #[test]
    fn token_shape_and_uniqueness() {
        let tokens: HashSet<String> = (0..200).map(|_| random_token(16).unwrap()).collect();
        assert_eq!(tokens.len(), 200);
        assert!(tokens.iter().all(|t| t.len() == 32
            && t.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())));
        assert_eq!(random_token(0).unwrap(), "");
    }

    #[test]
    fn alphanumeric_shape_and_coverage() {
        let s = random_alphanumeric(2000).unwrap();
        assert_eq!(s.len(), 2000);
        assert!(s.bytes().all(|b| b.is_ascii_alphanumeric()));
        // all 62 symbols should appear in 2000 draws (miss chance ~ 62 * e^-32)
        let distinct: HashSet<u8> = s.bytes().collect();
        assert_eq!(distinct.len(), 62);
        assert_eq!(random_alphanumeric(0).unwrap(), "");
    }
}
