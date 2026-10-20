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
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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

/// Whether `ip` is an address on the public internet. Loopback, private
/// (RFC 1918, unique local), link-local, CGNAT, multicast, documentation,
/// benchmarking and reserved ranges are not, and neither are IPv6 addresses
/// that embed such an IPv4 address (IPv4-mapped, NAT64, 6to4).
pub fn is_public_ip(ip: IpAddr) -> bool {
    fn public_v4(v4: Ipv4Addr) -> bool {
        let o = v4.octets();
        !(v4.is_loopback()
            || v4.is_private()
            || v4.is_link_local()
            || v4.is_unspecified()
            || v4.is_broadcast()
            || v4.is_multicast()
            || v4.is_documentation()
            || o[0] == 0                                  // 0.0.0.0/8
            || o[0] >= 240                                // reserved
            || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT 100.64.0.0/10
            || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking
            || (o[0] == 192 && o[1] == 0 && o[2] == 0)) // IETF protocol assignments
    }
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let embedded =
                |a: u16, b: u16| Ipv4Addr::new((a >> 8) as u8, a as u8, (b >> 8) as u8, b as u8);
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            // NAT64 64:ff9b::/96 carries an IPv4 address in the last 32 bits
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return public_v4(embedded(seg[6], seg[7]));
            }
            // 6to4 2002::/16 carries it right after the prefix
            if seg[0] == 0x2002 {
                return public_v4(embedded(seg[1], seg[2]));
            }
            // deprecated IPv4-compatible ::a.b.c.d
            if seg[..6] == [0, 0, 0, 0, 0, 0] {
                return false;
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00            // unique local fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80            // link local fe80::/10
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // documentation
        }
    }
}

/// Why [`resolve_public`] refused
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The name did not resolve to any address
    Resolution,
    /// At least one answer is not a public address
    NotPublic(IpAddr),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Resolution => f.write_str("name resolution failed"),
            ResolveError::NotPublic(ip) => write!(f, "target address {} is not public", ip),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve `host` and vet the answers before connecting (SSRF protection)
///
/// Every address the name resolves to must be public; with `allow_private`
/// the check is skipped (internal tools, tests). Connect to the returned
/// address itself, not to the name again, so a DNS answer that changes between
/// check and use cannot redirect the connection (DNS rebinding).
///
/// ```no_run
/// # async fn run() {
/// use ullun::security::resolve_public;
/// let addr = resolve_public("example.com", 443, false).await.unwrap();
/// let _stream = tokio::net::TcpStream::connect(addr).await.unwrap();
/// # }
/// ```
pub async fn resolve_public(
    host: &str,
    port: u16,
    allow_private: bool,
) -> std::result::Result<SocketAddr, ResolveError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| ResolveError::Resolution)?
        .collect();
    let first = addrs.first().copied().ok_or(ResolveError::Resolution)?;
    if !allow_private {
        if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
            return Err(ResolveError::NotPublic(bad.ip()));
        }
    }
    Ok(first)
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
    fn public_and_non_public_addresses() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.0.1",
            "198.20.0.1",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
            "64:ff9b::808:808",
            "2002:0808:0808::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public_ip(ip(public)), "{public} should be public");
        }
        for private in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "198.18.0.1",
            "198.19.255.255",
            "192.0.0.1",
            "::1",
            "::",
            "::1234",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "2002:7f00:1::1",
            "2002:c0a8:101::1",
        ] {
            assert!(!is_public_ip(ip(private)), "{private} should not be public");
        }
    }

    #[tokio::test]
    async fn resolve_public_vets_literals_and_names() {
        assert_eq!(
            resolve_public("93.184.216.34", 443, false).await,
            Ok("93.184.216.34:443".parse().unwrap())
        );
        assert_eq!(
            resolve_public("127.0.0.1", 80, false).await,
            Err(ResolveError::NotPublic("127.0.0.1".parse().unwrap()))
        );
        assert_eq!(
            resolve_public("169.254.169.254", 80, false)
                .await
                .unwrap_err()
                .to_string(),
            "target address 169.254.169.254 is not public"
        );
        // bracketed literals are not hostnames: resolvers disagree whether they parse
        // (Windows accepts "[::1]"), but none of them may yield a usable address
        assert!(resolve_public("[::1]", 80, false).await.is_err());
        assert_eq!(
            resolve_public("::1", 80, false).await,
            Err(ResolveError::NotPublic("::1".parse().unwrap()))
        );
        // the escape hatch for internal use
        assert_eq!(
            resolve_public("127.0.0.1", 80, true).await,
            Ok("127.0.0.1:80".parse().unwrap())
        );
        // names that do not resolve
        assert_eq!(
            resolve_public("no-such-host.invalid", 80, false).await,
            Err(ResolveError::Resolution)
        );
        // "localhost" resolves to loopback, which is not public
        assert!(matches!(
            resolve_public("localhost", 80, false).await,
            Err(ResolveError::NotPublic(_))
        ));
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
