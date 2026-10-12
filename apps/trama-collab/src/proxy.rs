//! Reverse-proxy trust: which TCP peers may vouch for the real client address.
//!
//! The service itself speaks plain HTTP. On the internet it sits behind a TLS
//! terminating proxy, so every TCP peer is the proxy and per-client logic
//! (login throttling) would collapse onto one address. A forwarding header is
//! therefore honored, but only when the TCP peer is inside a configured list
//! of trusted proxy networks, and then only the right-most hop that is not
//! itself a trusted proxy is used. Anything a client can write sits to the
//! left of the proxy's own entry and is never selected.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use axum::http::{HeaderMap, header::HeaderName};

/// At most this many right-most hops of a forwarding header are examined.
const MAX_HOPS: usize = 32;
/// Header values longer than this are ignored entirely (fail closed).
const MAX_HEADER_BYTES: usize = 2048;

static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
static FORWARDED: HeaderName = HeaderName::from_static("forwarded");

/// An IPv4 or IPv6 network in CIDR notation (`10.0.0.0/8`, `::1/128`, or a
/// bare address meaning a single host).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpCidr {
    network: u128,
    mask: u128,
    v6: bool,
}

impl IpCidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (normalize(ip), self.v6) {
            (IpAddr::V4(ip), false) => u128::from(u32::from(ip)) & self.mask == self.network,
            (IpAddr::V6(ip), true) => u128::from(ip) & self.mask == self.network,
            _ => false,
        }
    }
}

impl FromStr for IpCidr {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let ip: IpAddr = address
            .parse()
            .map_err(|_| format!("`{text}` is not an IP address or CIDR network"))?;
        let (bits, v6) = match normalize(ip) {
            IpAddr::V4(_) => (32_u32, false),
            IpAddr::V6(_) => (128_u32, true),
        };
        let prefix = match prefix {
            Some(prefix) => prefix
                .parse::<u32>()
                .ok()
                .filter(|prefix| *prefix <= bits)
                .ok_or_else(|| format!("`{text}` has an invalid prefix length"))?,
            None => bits,
        };
        let address = match normalize(ip) {
            IpAddr::V4(ip) => u128::from(u32::from(ip)),
            IpAddr::V6(ip) => u128::from(ip),
        };
        // `0.0.0.0/0` (or `::/0`) would let every peer set its own client
        // address; refuse it instead of silently disabling the protection.
        if prefix == 0 {
            return Err(format!(
                "`{text}` would trust every peer; list the proxy networks explicitly"
            ));
        }
        let all = if bits == 128 {
            !0_u128
        } else {
            (1_u128 << bits) - 1
        };
        let host = bits - prefix;
        let host_mask = if host == 128 {
            !0_u128
        } else {
            (1_u128 << host) - 1
        };
        let mask = all & !host_mask;
        Ok(Self {
            network: address & mask,
            mask,
            v6,
        })
    }
}

/// Which forwarding header the proxy is configured to maintain. Only the
/// selected header is ever read: a proxy that appends to one header passes any
/// client-supplied copy of the other through untouched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForwardedHeader {
    /// `X-Forwarded-For: client, proxy1` (Caddy, nginx, Traefik, HAProxy).
    #[default]
    XForwardedFor,
    /// RFC 7239 `Forwarded: for=client, for=proxy1`.
    Forwarded,
}

impl ForwardedHeader {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::XForwardedFor => "x-forwarded-for",
            Self::Forwarded => "forwarded",
        }
    }
}

impl FromStr for ForwardedHeader {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "x-forwarded-for" => Ok(Self::XForwardedFor),
            "forwarded" => Ok(Self::Forwarded),
            _ => Err("expected `x-forwarded-for` or `forwarded`".into()),
        }
    }
}

/// The trusted-proxy policy applied to every request.
#[derive(Clone, Debug)]
pub struct ProxyTrust {
    trusted: Vec<IpCidr>,
    header: ForwardedHeader,
}

impl ProxyTrust {
    pub fn new(trusted: Vec<IpCidr>, header: ForwardedHeader) -> Self {
        Self { trusted, header }
    }

    /// Loopback, RFC 1918 and IPv6 unique-local ranges: the networks a
    /// co-located proxy or a Docker Compose network uses.
    pub fn default_networks() -> Vec<IpCidr> {
        [
            "127.0.0.0/8",
            "::1/128",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "fc00::/7",
        ]
        .iter()
        .map(|network| network.parse().expect("built-in network is valid"))
        .collect()
    }

    pub fn trusted(&self) -> &[IpCidr] {
        &self.trusted
    }

    pub fn header(&self) -> ForwardedHeader {
        self.header
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|network| network.contains(ip))
    }

    /// The address login throttling should be keyed on.
    ///
    /// `peer` is the TCP peer. It is returned unchanged unless it is a
    /// trusted proxy. For a trusted peer the hop list is the forwarding header
    /// followed by the peer; trusted hops are skipped from the right and the
    /// first untrusted one is the client. A malformed or oversized header
    /// yields the peer itself, never a client-chosen value.
    pub fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = normalize(peer);
        if !self.is_trusted(peer) {
            return peer;
        }
        let hops = match self.header {
            ForwardedHeader::XForwardedFor => parse_x_forwarded_for(headers),
            ForwardedHeader::Forwarded => parse_forwarded(headers),
        };
        let Some(hops) = hops else {
            return peer;
        };
        let mut candidate = peer;
        for hop in hops.into_iter().rev() {
            match hop {
                Some(ip) if self.is_trusted(ip) => candidate = ip,
                Some(ip) => return ip,
                // An unparseable hop cannot be attributed; stop at the last
                // address a trusted party vouched for.
                None => return candidate,
            }
        }
        candidate
    }
}

/// Maps `::ffff:a.b.c.d` to the IPv4 address so keys and CIDRs agree.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// One entry per hop, left to right; `None` for an entry that is not an IP.
type Hops = Vec<Option<IpAddr>>;

fn joined_header(headers: &HeaderMap, name: &HeaderName) -> Option<Option<String>> {
    let mut values = headers.get_all(name).iter().peekable();
    values.peek()?;
    let mut joined = String::new();
    for value in values {
        let Ok(text) = value.to_str() else {
            return Some(None);
        };
        if !joined.is_empty() {
            joined.push(',');
        }
        joined.push_str(text);
        if joined.len() > MAX_HEADER_BYTES {
            return Some(None);
        }
    }
    Some(Some(joined))
}

fn parse_x_forwarded_for(headers: &HeaderMap) -> Option<Hops> {
    let joined = joined_header(headers, &X_FORWARDED_FOR)??;
    Some(tail(
        joined.split(',').map(|entry| parse_node(entry)).collect(),
    ))
}

fn parse_forwarded(headers: &HeaderMap) -> Option<Hops> {
    let joined = joined_header(headers, &FORWARDED)??;
    let hops = joined
        .split(',')
        .map(|element| {
            element
                .split(';')
                .find_map(|pair| {
                    let (name, value) = pair.split_once('=')?;
                    name.trim()
                        .eq_ignore_ascii_case("for")
                        .then(|| parse_node(value))
                })
                // An element without `for=` is an unknown hop.
                .unwrap_or(None)
        })
        .collect();
    Some(tail(hops))
}

fn tail(mut hops: Hops) -> Hops {
    if hops.len() > MAX_HOPS {
        hops.drain(..hops.len() - MAX_HOPS);
    }
    hops
}

/// Parses `1.2.3.4`, `1.2.3.4:5678`, `2001:db8::1`, `[2001:db8::1]:5678`,
/// optionally quoted. Obfuscated identifiers (`_hidden`) and `unknown` are
/// `None`. IPv6 scope ids (`%eth0`) are not accepted.
fn parse_node(raw: &str) -> Option<IpAddr> {
    let node = raw.trim().trim_matches('"').trim();
    if node.is_empty() {
        return None;
    }
    if let Ok(ip) = node.parse::<IpAddr>() {
        return Some(normalize(ip));
    }
    if let Some(rest) = node.strip_prefix('[') {
        let (address, tail) = rest.split_once(']')?;
        if !(tail.is_empty() || tail.strip_prefix(':').is_some_and(valid_port)) {
            return None;
        }
        return address
            .parse::<Ipv6Addr>()
            .ok()
            .map(|ip| normalize(ip.into()));
    }
    let (address, port) = node.rsplit_once(':')?;
    if !valid_port(port) {
        return None;
    }
    address.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn trust(header: ForwardedHeader) -> ProxyTrust {
        ProxyTrust::new(ProxyTrust::default_networks(), header)
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn headers(name: &'static str, values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn cidr_matching_covers_v4_v6_and_mapped_addresses() {
        let net: IpCidr = "172.16.0.0/12".parse().unwrap();
        assert!(net.contains(ip("172.31.255.255")));
        assert!(!net.contains(ip("172.32.0.1")));
        assert!(net.contains(ip("::ffff:172.20.0.5")));
        let host: IpCidr = "203.0.113.7".parse().unwrap();
        assert!(host.contains(ip("203.0.113.7")));
        assert!(!host.contains(ip("203.0.113.8")));
        let v6: IpCidr = "fc00::/7".parse().unwrap();
        assert!(v6.contains(ip("fd12:3456::1")));
        assert!(!v6.contains(ip("2001:db8::1")));
        assert!(!v6.contains(ip("10.0.0.1")));
    }

    #[test]
    fn cidr_rejects_garbage_and_match_everything_networks() {
        for bad in [
            "",
            "nope",
            "10.0.0.0/33",
            "::/129",
            "10.0.0.0/-1",
            "0.0.0.0/0",
            "::/0",
        ] {
            assert!(bad.parse::<IpCidr>().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn untrusted_peer_cannot_set_its_address() {
        let policy = trust(ForwardedHeader::XForwardedFor);
        let spoofed = headers("x-forwarded-for", &["10.0.0.1"]);
        assert_eq!(
            policy.client_ip(ip("203.0.113.9"), &spoofed),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn trusted_proxy_forwarded_address_is_used() {
        let policy = trust(ForwardedHeader::XForwardedFor);
        let map = headers("x-forwarded-for", &["198.51.100.4"]);
        assert_eq!(policy.client_ip(ip("172.18.0.2"), &map), ip("198.51.100.4"));
        // No header at all: the proxy itself.
        assert_eq!(
            policy.client_ip(ip("172.18.0.2"), &HeaderMap::new()),
            ip("172.18.0.2")
        );
    }

    #[test]
    fn rightmost_untrusted_hop_wins_over_client_supplied_prefix() {
        let policy = trust(ForwardedHeader::XForwardedFor);
        // The client sent `9.9.9.9`; the edge proxy appended the real peer;
        // an inner trusted proxy appended itself.
        let map = headers("x-forwarded-for", &["9.9.9.9, 198.51.100.4, 10.1.1.1"]);
        assert_eq!(policy.client_ip(ip("172.18.0.2"), &map), ip("198.51.100.4"));
        // Repeated header lines are treated as one comma-joined list.
        let map = headers("x-forwarded-for", &["9.9.9.9", "198.51.100.4"]);
        assert_eq!(policy.client_ip(ip("172.18.0.2"), &map), ip("198.51.100.4"));
        // Everything trusted: the left-most is the origin.
        let map = headers("x-forwarded-for", &["10.5.5.5, 192.168.1.1"]);
        assert_eq!(policy.client_ip(ip("127.0.0.1"), &map), ip("10.5.5.5"));
    }

    #[test]
    fn malformed_or_oversized_headers_fall_back_to_the_peer() {
        let policy = trust(ForwardedHeader::XForwardedFor);
        for value in ["garbage", "unknown", "_hidden", "198.51.100.4, unknown"] {
            // An unparseable right-most hop stops the walk at the peer.
            let map = headers("x-forwarded-for", &[value]);
            assert_eq!(
                policy.client_ip(ip("172.18.0.2"), &map),
                ip("172.18.0.2"),
                "{value}"
            );
        }
        let huge = "1.1.1.1,".repeat(1000);
        let map = headers("x-forwarded-for", &[&huge]);
        assert_eq!(policy.client_ip(ip("172.18.0.2"), &map), ip("172.18.0.2"));
    }

    #[test]
    fn only_the_configured_header_is_read() {
        let xff = trust(ForwardedHeader::XForwardedFor);
        let rfc = trust(ForwardedHeader::Forwarded);
        let mut map = headers("forwarded", &["for=1.1.1.1"]);
        map.append("x-forwarded-for", HeaderValue::from_static("198.51.100.4"));
        assert_eq!(xff.client_ip(ip("10.0.0.2"), &map), ip("198.51.100.4"));
        assert_eq!(rfc.client_ip(ip("10.0.0.2"), &map), ip("1.1.1.1"));
    }

    #[test]
    fn rfc7239_forwarded_values_parse() {
        let rfc = trust(ForwardedHeader::Forwarded);
        let map = headers(
            "forwarded",
            &[r#"for=203.0.113.43;proto=https, for="[2001:db8:cafe::17]:4711";by=10.0.0.2"#],
        );
        assert_eq!(rfc.client_ip(ip("10.0.0.2"), &map), ip("2001:db8:cafe::17"));
        let map = headers("forwarded", &["for=198.51.100.4:6000"]);
        assert_eq!(rfc.client_ip(ip("10.0.0.2"), &map), ip("198.51.100.4"));
        let map = headers("forwarded", &["for=_secret"]);
        assert_eq!(rfc.client_ip(ip("10.0.0.2"), &map), ip("10.0.0.2"));
    }

    #[test]
    fn custom_trusted_list_replaces_the_defaults() {
        let policy = ProxyTrust::new(
            vec!["203.0.113.0/24".parse().unwrap()],
            ForwardedHeader::XForwardedFor,
        );
        let map = headers("x-forwarded-for", &["198.51.100.4"]);
        // A private-range peer is no longer trusted...
        assert_eq!(policy.client_ip(ip("10.0.0.2"), &map), ip("10.0.0.2"));
        // ...the configured one is.
        assert_eq!(
            policy.client_ip(ip("203.0.113.5"), &map),
            ip("198.51.100.4")
        );
    }
}
