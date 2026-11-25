//! Peer addresses: an IP address or hostname with an optional port.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

/// Where to reach a peer. The host is resolved by the transport at
/// runtime, not here, so a hostname that doesn't resolve yet is still a
/// valid config.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerAddress {
    host: Host,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Host {
    Ip(IpAddr),
    /// Lowercased, so duplicate detection is case-insensitive.
    Name(String),
}

/// Why an address string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("is empty")]
    Empty,
    #[error("has an invalid port {0:?}")]
    Port(String),
    #[error("{0:?} is not an IP address or hostname")]
    Host(String),
}

impl PeerAddress {
    /// Parses `host`, `host:port`, `ip`, `ip:port`, `[ipv6]`, `[ipv6]:port`,
    /// or a bare IPv6 address. `default_port` is used when none is given.
    pub fn parse(text: &str, default_port: u16) -> Result<Self, AddressError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(AddressError::Empty);
        }

        // Bare IPv6 ("fe80::1") has colons but no port.
        if let Ok(ip) = text.parse::<Ipv6Addr>() {
            return Ok(Self {
                host: Host::Ip(IpAddr::V6(ip)),
                port: default_port,
            });
        }

        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (inner, after) = rest
                .split_once(']')
                .ok_or_else(|| AddressError::Host(text.to_owned()))?;
            let ip: Ipv6Addr = inner
                .parse()
                .map_err(|_| AddressError::Host(inner.to_owned()))?;
            let port = match after {
                "" => None,
                _ => Some(
                    after
                        .strip_prefix(':')
                        .ok_or_else(|| AddressError::Host(text.to_owned()))?,
                ),
            };
            (Host::Ip(IpAddr::V6(ip)), port)
        } else {
            let (host, port) = match text.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (text, None),
            };
            (parse_host(host)?, port)
        };

        let port = match port {
            None => default_port,
            Some(p) => match p.parse::<u16>() {
                Ok(n) if n != 0 => n,
                _ => return Err(AddressError::Port(p.to_owned())),
            },
        };
        Ok(Self { host, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The host part, in a form `ToSocketAddrs` accepts together with
    /// [`port`](Self::port).
    pub fn host(&self) -> String {
        match &self.host {
            Host::Ip(ip) => ip.to_string(),
            Host::Name(name) => name.clone(),
        }
    }
}

fn parse_host(host: &str) -> Result<Host, AddressError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(Host::Ip(ip));
    }
    // RFC 1123 hostname: dot-separated labels of letters, digits, and
    // hyphens, not starting or ending with a hyphen.
    let valid = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if valid {
        Ok(Host::Name(host.to_ascii_lowercase()))
    } else {
        Err(AddressError::Host(host.to_owned()))
    }
}

impl fmt::Display for PeerAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
            Host::Ip(IpAddr::V4(ip)) => write!(f, "{ip}:{}", self.port),
            Host::Name(name) => write!(f, "{name}:{}", self.port),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<PeerAddress, AddressError> {
        PeerAddress::parse(s, 4777)
    }

    #[test]
    fn accepts_all_forms() {
        let cases = [
            ("192.168.1.20", "192.168.1.20:4777"),
            ("192.168.1.20:5000", "192.168.1.20:5000"),
            ("fe80::1", "[fe80::1]:4777"),
            ("[fe80::1]", "[fe80::1]:4777"),
            ("[fe80::1]:5000", "[fe80::1]:5000"),
            ("Desk.LAN", "desk.lan:4777"),
            ("desk-2:5000", "desk-2:5000"),
            ("  10.0.0.1  ", "10.0.0.1:4777"),
        ];
        for (input, want) in cases {
            let addr = parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(addr.to_string(), want, "{input}");
        }
    }

    #[test]
    fn display_round_trips() {
        for input in ["10.0.0.1:9", "[::1]:9", "host.example:9"] {
            let addr = parse(input).unwrap();
            assert_eq!(parse(&addr.to_string()).unwrap(), addr);
        }
    }

    #[test]
    fn rejects_bad_addresses() {
        assert_eq!(parse(""), Err(AddressError::Empty));
        assert!(matches!(parse("host:0"), Err(AddressError::Port(_))));
        assert!(matches!(parse("host:70000"), Err(AddressError::Port(_))));
        assert!(matches!(parse("host:"), Err(AddressError::Port(_))));
        assert!(matches!(parse("[::1]x"), Err(AddressError::Host(_))));
        assert!(matches!(parse("[nope]:1"), Err(AddressError::Host(_))));
        assert!(matches!(parse("bad_host"), Err(AddressError::Host(_))));
        assert!(matches!(parse("-bad.lan"), Err(AddressError::Host(_))));
        assert!(matches!(parse("a..b"), Err(AddressError::Host(_))));
        assert!(matches!(parse("a b"), Err(AddressError::Host(_))));
    }
}
