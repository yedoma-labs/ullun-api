//! Trusted reverse proxies and the real client address
//!
//! Behind a load balancer every connection comes from the proxy, and the client's
//! address travels in `X-Forwarded-For` / `Forwarded`. Those headers are written
//! by *clients* too: a naive "take the first entry" lets anyone choose their own
//! address, which defeats rate limits, allow-lists and audit logs. The rule that
//! works: only believe the header when the connection comes from a proxy you
//! run, and read it from the right, skipping hops that are your own proxies.
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::proxy::TrustedProxies;
//!
//! # async fn run() {
//! App::new()
//!     // the load balancers in front of this service
//!     .trusted_proxies(TrustedProxies::new().trust("10.0.0.0/8").unwrap())
//!     .get("/ip", |req: Request| async move {
//!         Ok(Response::text(req.client_ip().map(|ip| ip.to_string()).unwrap_or_default()))
//!     })
//!     .run("0.0.0.0:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! With trusted proxies configured:
//!
//! * [`Request::client_ip`] is the first address from the right that is not one
//!   of your proxies (`Forwarded` wins over `X-Forwarded-For`); from any other
//!   peer it is the peer address itself, whatever the headers say
//! * [`Request::is_secure`] also accepts `X-Forwarded-Proto: https` /
//!   `Forwarded: proto=https` from a trusted peer
//! * the default [`RateLimiter`](crate::rate_limit::RateLimiter) key and the
//!   [`AccessLog`](crate::observability::AccessLog) use the client address
//!
//! Without them nothing changes: `client_ip()` is the peer address.

use crate::request::Request;
use std::net::IpAddr;
use std::str::FromStr;

/// A network such as `10.0.0.0/8` or `fd00::/8` (a bare address is a single host)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("invalid address in {s:?}"))?;
        let addr = addr.to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("invalid prefix length in {s:?}"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_matches(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_matches(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

fn prefix_matches(net: &[u8], ip: &[u8], prefix: u8) -> bool {
    let full = (prefix / 8) as usize;
    if net[..full] != ip[..full] {
        return false;
    }
    let rest = prefix % 8;
    rest == 0 || {
        let mask = 0xffu8 << (8 - rest);
        net[full] & mask == ip[full] & mask
    }
}

/// The proxies whose forwarding headers are believed
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    nets: Vec<Cidr>,
}

impl TrustedProxies {
    /// Nobody is trusted yet
    pub fn new() -> Self {
        Self::default()
    }

    /// Trust a network or address, e.g. `"10.0.0.0/8"`, `"203.0.113.7"`, `"fd00::/8"`
    pub fn trust(mut self, cidr: &str) -> Result<Self, String> {
        self.nets.push(cidr.parse()?);
        self.nets.dedup();
        Ok(self)
    }

    /// Loopback, private (RFC 1918), link-local and unique-local addresses: a
    /// proxy inside your own network. Do not use it when untrusted machines can
    /// reach the service from such an address.
    pub fn private_networks() -> Self {
        let mut t = Self::new();
        for net in [
            "127.0.0.0/8",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "::1/128",
            "fc00::/7",
            "fe80::/10",
        ] {
            t.nets.push(net.parse().expect("built-in network"));
        }
        t
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(ip))
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }
}

/// Address inside a `for=` / `X-Forwarded-For` element: plain, with port,
/// bracketed IPv6, quoted
fn parse_hop(raw: &str) -> Option<IpAddr> {
    let s = raw.trim().trim_matches('"');
    if let Some(rest) = s.strip_prefix('[') {
        return rest.split(']').next()?.parse().ok();
    }
    if let Ok(ip) = s.parse() {
        return Some(ip);
    }
    // ip:port (IPv4 only; a bare IPv6 address has several colons)
    let (host, port) = s.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    host.parse().ok()
}

/// Elements of the `Forwarded` header as (for, proto) per hop
fn forwarded_hops(value: &str) -> Vec<(Option<String>, Option<String>)> {
    value
        .split(',')
        .map(|element| {
            let mut for_ = None;
            let mut proto = None;
            for pair in element.split(';') {
                if let Some((k, v)) = pair.split_once('=') {
                    let v = v.trim().trim_matches('"').to_string();
                    match k.trim().to_ascii_lowercase().as_str() {
                        "for" => for_ = Some(v),
                        "proto" => proto = Some(v.to_ascii_lowercase()),
                        _ => {}
                    }
                }
            }
            (for_, proto)
        })
        .collect()
}

impl Request {
    /// The address of the real client, see [`crate::proxy`]. `None` only when
    /// the request has no peer address at all.
    pub fn client_ip(&self) -> Option<IpAddr> {
        let peer = self.remote_addr?.ip();
        let Some(trusted) = self.trusted_proxies.as_deref() else {
            return Some(peer);
        };
        if !trusted.contains(peer) {
            return Some(peer);
        }
        let hops: Vec<String> = match self.headers.get("forwarded").and_then(|v| v.to_str().ok()) {
            Some(f) => forwarded_hops(f)
                .into_iter()
                .map(|(for_, _)| for_.unwrap_or_default())
                .collect(),
            None => self
                .headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(|h| h.trim().to_string())
                .collect(),
        };
        let mut known = peer;
        for hop in hops.iter().rev() {
            match parse_hop(hop) {
                Some(ip) if trusted.contains(ip) => known = ip,
                Some(ip) => return Some(ip),
                // "unknown" / obfuscated identifiers: nothing more can be learned
                None => return Some(known),
            }
        }
        Some(known)
    }

    /// Scheme announced by a trusted proxy: `Some(true)` for https
    pub(crate) fn forwarded_https(&self) -> bool {
        let (Some(trusted), Some(peer)) = (self.trusted_proxies.as_deref(), self.remote_addr)
        else {
            return false;
        };
        if !trusted.contains(peer.ip()) {
            return false;
        }
        // the hop closest to us is the one our own proxy wrote
        if let Some(f) = self.headers.get("forwarded").and_then(|v| v.to_str().ok()) {
            if let Some((_, Some(proto))) = forwarded_hops(f).into_iter().next_back() {
                return proto == "https";
            }
        }
        self.headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_matching() {
        let net: Cidr = "10.1.0.0/16".parse().unwrap();
        assert!(net.contains(ip("10.1.200.3")));
        assert!(!net.contains(ip("10.2.0.1")));
        let odd: Cidr = "172.16.0.0/12".parse().unwrap();
        assert!(odd.contains(ip("172.31.255.255")));
        assert!(!odd.contains(ip("172.32.0.1")));
        let host: Cidr = "203.0.113.7".parse().unwrap();
        assert!(host.contains(ip("203.0.113.7")) && !host.contains(ip("203.0.113.8")));
        let v6: Cidr = "fd00::/8".parse().unwrap();
        assert!(v6.contains(ip("fd12::1")) && !v6.contains(ip("fe80::1")));
        // families do not mix, mapped addresses are unwrapped
        assert!(!net.contains(ip("fd00::1")));
        assert!(net.contains(ip("::ffff:10.1.0.9")));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(ip("8.8.8.8")));
    }

    #[test]
    fn cidr_rejects_garbage() {
        for bad in [
            "",
            "10.0.0.0/33",
            "10.0.0.0/x",
            "nope",
            "::1/129",
            "10.0.0/8",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad}");
        }
    }

    #[test]
    fn hop_parsing() {
        assert_eq!(parse_hop("203.0.113.9"), Some(ip("203.0.113.9")));
        assert_eq!(parse_hop(" 203.0.113.9:4711 "), Some(ip("203.0.113.9")));
        assert_eq!(parse_hop("\"[2001:db8::1]:4711\""), Some(ip("2001:db8::1")));
        assert_eq!(parse_hop("2001:db8::1"), Some(ip("2001:db8::1")));
        assert_eq!(parse_hop("unknown"), None);
        assert_eq!(parse_hop("_hidden"), None);
        assert_eq!(parse_hop(""), None);
    }

    #[test]
    fn private_networks_preset() {
        let t = TrustedProxies::private_networks();
        for private in [
            "127.0.0.1",
            "10.9.9.9",
            "192.168.1.1",
            "172.20.0.1",
            "::1",
            "fd00::5",
        ] {
            assert!(t.contains(ip(private)), "{private}");
        }
        for public in ["8.8.8.8", "172.32.0.1", "2001:db8::1"] {
            assert!(!t.contains(ip(public)), "{public}");
        }
        assert!(TrustedProxies::new().is_empty());
    }
}
