//! What the host's side of a bottle's network decides itself about each connection the bottle makes,
//! as bottled's decider would (Gateway.decide in bottle/egress.py), from the policy bottled gives
//! it (Gateway.driver_policy): the connections bottled must still see, which are handed to it, and
//! the private addresses the bottle may reach by name. Everything else is connected from here, or
//! refused, without asking.
//!
//! A connection is handed to bottled when it goes to an address or network the policy names, or
//! when its first bytes give a name the policy names on that port: a TLS ClientHello's server name,
//! or an HTTP request's Host. One whose name can't be told yet (nothing sent, or part of a TLS
//! record) is handed over too when any name is to be handed on its port, as bottled then waits for
//! the rest. Otherwise: loopback, link-local, multicast and unspecified addresses are refused;
//! public ones are connected; a private one only by a name `allow_private` matches, that leads
//! there. IPv6 is always handed over: the bottle has no route for it, and bottled judges it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub allow_private: Vec<String>,
    #[serde(default)]
    pub hand: Vec<Hand>,
}

/// A connection bottled must see: to an address, into a network, or by name, each on a port.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Hand {
    Address { address: IpAddr, port: u16 },
    Network { network: String, port: u16 },
    Name { name: String, port: u16 },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Local {
    /// bottled decides: ask it.
    Hand,
    Direct,
    Deny,
}

impl Policy {
    /// What to do with a connection to `to` whose first bytes are `first`; `resolve` gives a name's
    /// addresses, for a private address reached by name.
    pub fn decide(
        &self,
        to: SocketAddr,
        first: &[u8],
        resolve: impl Fn(&str) -> Vec<IpAddr>,
    ) -> Local {
        let ip = match to.ip() {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => return Local::Hand,
            },
            v4 => v4,
        };
        let port = to.port();
        for rule in &self.hand {
            match rule {
                Hand::Address { address, port: p } if *address == ip && *p == port => {
                    return Local::Hand;
                }
                Hand::Network { network, port: p } if *p == port && in_network(ip, network) => {
                    return Local::Hand;
                }
                _ => {}
            }
        }
        let named_here = |name: &str| {
            self.hand.iter().any(|rule| {
                matches!(rule, Hand::Name { name: pattern, port: p } if *p == port && glob(pattern, name))
            })
        };
        let name = name_in(first);
        match &name {
            Some(name) if named_here(name) => return Local::Hand,
            None if opening_unread(first)
                && self
                    .hand
                    .iter()
                    .any(|r| matches!(r, Hand::Name { port: p, .. } if *p == port)) =>
            {
                return Local::Hand;
            }
            _ => {}
        }
        let IpAddr::V4(v4) = ip else {
            return Local::Hand;
        };
        if v4.is_loopback() || v4.is_link_local() || v4.is_multicast() || v4.is_unspecified() {
            return Local::Deny;
        }
        if is_global(v4) {
            return Local::Direct;
        }
        // A private address: only by a name allowed private, that leads there.
        match name {
            Some(name)
                if self.allow_private.iter().any(|p| glob(p, &name))
                    && resolve(&name).contains(&ip) =>
            {
                Local::Direct
            }
            _ => Local::Deny,
        }
    }
}

/// Whether `ip` is in `network`, written ADDRESS/PREFIX (IPv4).
fn in_network(ip: IpAddr, network: &str) -> bool {
    let (IpAddr::V4(ip), Some((base, prefix))) = (ip, network.split_once('/')) else {
        return false;
    };
    let (Ok(base), Ok(prefix)) = (base.parse::<Ipv4Addr>(), prefix.parse::<u32>()) else {
        return false;
    };
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix.min(32))
    };
    u32::from(ip) & mask == u32::from(base) & mask
}

/// Whether an IPv4 address is on the public internet, as Python's ipaddress has it: none of the
/// private, shared, reserved and documentation ranges.
fn is_global(ip: Ipv4Addr) -> bool {
    const NOT_GLOBAL: [(u32, u32); 16] = [
        (0x0000_0000, 8),  // 0.0.0.0/8
        (0x0A00_0000, 8),  // 10.0.0.0/8
        (0x6440_0000, 10), // 100.64.0.0/10, shared (CGNAT)
        (0x7F00_0000, 8),  // 127.0.0.0/8
        (0xA9FE_0000, 16), // 169.254.0.0/16
        (0xAC10_0000, 12), // 172.16.0.0/12
        (0xC000_0000, 29), // 192.0.0.0/29
        (0xC000_00AA, 31), // 192.0.0.170/31
        (0xC000_0200, 24), // 192.0.2.0/24, documentation
        (0xC0A8_0000, 16), // 192.168.0.0/16
        (0xC612_0000, 15), // 198.18.0.0/15, benchmarking
        (0xC633_6400, 24), // 198.51.100.0/24, documentation
        (0xCB00_7100, 24), // 203.0.113.0/24, documentation
        (0xF000_0000, 4),  // 240.0.0.0/4, reserved
        (0xFFFF_FFFF, 32), // broadcast
        (0xC000_0000, 24), // 192.0.0.0/24, protocol assignments
    ];
    let ip = u32::from(ip);
    !NOT_GLOBAL
        .iter()
        .any(|&(base, prefix)| ip & (u32::MAX << (32 - prefix)) == base)
}

/// Whether `name` matches `pattern`, as fnmatch does, without case: `*` any run of characters, `?`
/// one.
fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (
        pattern.to_lowercase().chars().collect(),
        name.to_lowercase().chars().collect(),
    );
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || p[i] == n[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

/// Whether `first` may be the start of a ClientHello not all here yet: nothing, or part of a TLS
/// record.
fn opening_unread(first: &[u8]) -> bool {
    first.is_empty() || (first[0] == 0x16 && !record_complete(first))
}

fn record_complete(data: &[u8]) -> bool {
    data.len() >= 5 && data.len() >= 5 + usize::from(u16::from_be_bytes([data[3], data[4]]))
}

/// The destination's name in a connection's first bytes: a ClientHello's server name, or an HTTP
/// request's Host.
pub fn name_in(first: &[u8]) -> Option<String> {
    if first.first() == Some(&0x16) {
        sni(first)
    } else {
        http_host(first)
    }
}

/// The server name in a TLS ClientHello record. What the bottle sent, so every length is checked
/// against what's there before it's followed.
fn sni(record: &[u8]) -> Option<String> {
    if record.len() < 9 || record[0] != 0x16 || record[5] != 0x01 {
        return None;
    }
    let end = (5 + usize::from(u16::from_be_bytes([record[3], record[4]]))).min(record.len());
    let body = record.get(9..end)?;
    let u16_at = |at: usize| -> Option<usize> {
        Some(usize::from(u16::from_be_bytes([
            *body.get(at)?,
            *body.get(at + 1)?,
        ])))
    };
    let mut at = 2 + 32; // client version, random
    at += 1 + usize::from(*body.get(at)?); // session id
    at += 2 + u16_at(at)?; // cipher suites
    at += 1 + usize::from(*body.get(at)?); // compression methods
    let extensions_end = body.len().min(at + 2 + u16_at(at)?);
    at += 2;
    while at + 4 <= extensions_end {
        let (kind, length) = (u16_at(at)?, u16_at(at + 2)?);
        at += 4;
        if kind == 0 {
            // server_name: a list of (type, name)
            let names = body.get(at..(at + length).min(body.len()))?;
            let mut inner = 2;
            while inner + 3 <= names.len() {
                let name_type = names[inner];
                let name_length =
                    usize::from(u16::from_be_bytes([names[inner + 1], names[inner + 2]]));
                let name = names.get(inner + 3..inner + 3 + name_length);
                if name_type == 0
                    && let Some(name) = name
                {
                    return hostname(name);
                }
                inner += 3 + name_length;
            }
            return None;
        }
        at += length;
    }
    None
}

/// The Host of an HTTP request whose head starts `head`, without its port.
fn http_host(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok().or_else(|| {
        // Cut off mid-character: what's whole of it.
        std::str::from_utf8(&head[..std::str::from_utf8(head).err()?.valid_up_to()]).ok()
    })?;
    let mut lines: Vec<&str> = text.split("\r\n").collect();
    let request_line: Vec<&str> = lines.first()?.split(' ').collect();
    if request_line.len() != 3 || !request_line[2].starts_with("HTTP/") {
        return None;
    }
    lines.pop(); // the last may be cut off
    for line in lines.iter().skip(1) {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        if name.trim().eq_ignore_ascii_case("host") {
            let value = value.trim();
            let value = if let Some(rest) = value.strip_prefix('[') {
                rest.split_once(']').map_or("", |(v6, _)| v6)
            } else {
                value.rsplit_once(':').map_or(value, |(h, _)| h)
            };
            return hostname(value.as_bytes());
        }
    }
    None
}

fn hostname(name: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(name)
        .ok()?
        .trim_end_matches('.')
        .to_lowercase();
    let ok = !text.is_empty()
        && text.len() <= 253
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-.:_".contains(c));
    ok.then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(json: &str) -> Policy {
        serde_json::from_str(json).unwrap()
    }

    fn to(address: &str, port: u16) -> SocketAddr {
        SocketAddr::new(address.parse().unwrap(), port)
    }

    /// A ClientHello for `name`, as a TLS client sends it.
    fn client_hello(name: &str) -> Vec<u8> {
        let mut server_name = vec![0];
        server_name.extend_from_slice(&(name.len() as u16).to_be_bytes());
        server_name.extend_from_slice(name.as_bytes());
        let mut list = (server_name.len() as u16).to_be_bytes().to_vec();
        list.extend(server_name);
        let mut extension = vec![0, 0];
        extension.extend_from_slice(&(list.len() as u16).to_be_bytes());
        extension.extend(list);
        let mut body = vec![3, 3];
        body.extend([0; 32]);
        body.push(0); // session id
        body.extend([0, 2, 0x13, 0x01]); // one cipher suite
        body.extend([1, 0]); // compression: none
        body.extend_from_slice(&(extension.len() as u16).to_be_bytes());
        body.extend(extension);
        let mut handshake = vec![1, 0];
        handshake.extend_from_slice(&(body.len() as u16).to_be_bytes());
        handshake.extend(body);
        let mut record = vec![0x16, 3, 1];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend(handshake);
        record
    }

    const POLICY: &str = r#"{"version": 1, "allow_private": ["*.corp.example"], "hand": [
        {"address": "192.0.2.1", "port": 80}, {"network": "192.0.2.0/24", "port": 22},
        {"name": "github.com", "port": 443}, {"name": "*.maven.example", "port": 443},
        {"name": "deb.debian.org", "port": 80}]}"#;

    #[test]
    fn what_bottled_must_see_is_handed_to_it() {
        let p = policy(POLICY);
        let nothing = |_: &str| vec![];
        assert_eq!(p.decide(to("192.0.2.1", 80), b"", nothing), Local::Hand);
        assert_eq!(p.decide(to("192.0.2.7", 22), b"", nothing), Local::Hand);
        assert_eq!(
            p.decide(
                to("140.82.112.3", 443),
                &client_hello("github.com"),
                nothing
            ),
            Local::Hand
        );
        assert_eq!(
            p.decide(
                to("1.2.3.4", 443),
                &client_hello("repo.maven.example"),
                nothing
            ),
            Local::Hand
        );
        let get = b"GET /debian/pool/x.deb HTTP/1.1\r\nHost: deb.debian.org\r\n\r\n";
        assert_eq!(p.decide(to("151.101.2.132", 80), get, nothing), Local::Hand);
        // Its name not told yet, on a port names are handed on: bottled waits for it.
        assert_eq!(
            p.decide(
                to("140.82.112.3", 443),
                &client_hello("github.com")[..20],
                nothing
            ),
            Local::Hand
        );
        assert_eq!(p.decide(to("140.82.112.3", 443), b"", nothing), Local::Hand);
        assert_eq!(p.decide(to("::1", 443), b"", nothing), Local::Hand); // IPv6: bottled's
    }

    #[test]
    fn the_rest_is_decided_here() {
        let p = policy(POLICY);
        let nothing = |_: &str| vec![];
        assert_eq!(
            p.decide(
                to("93.184.216.34", 443),
                &client_hello("example.com"),
                nothing
            ),
            Local::Direct
        );
        assert_eq!(
            p.decide(to("93.184.216.34", 22), b"", nothing),
            Local::Direct
        ); // nothing named on 22
        assert_eq!(
            p.decide(
                to("140.82.112.3", 8080),
                &client_hello("github.com"),
                nothing
            ),
            Local::Direct
        );
        for refused in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
        ] {
            assert_eq!(
                p.decide(
                    to(refused, 80),
                    b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
                    nothing
                ),
                Local::Deny,
                "{refused}"
            );
        }
        // A private address by a name allowed private that leads there; not by one that doesn't.
        let corp = |name: &str| {
            if name == "git.corp.example" {
                vec!["10.1.2.3".parse().unwrap()]
            } else {
                vec![]
            }
        };
        assert_eq!(
            p.decide(to("10.1.2.3", 443), &client_hello("git.corp.example"), corp),
            Local::Direct
        );
        assert_eq!(
            p.decide(to("10.9.9.9", 443), &client_hello("git.corp.example"), corp),
            Local::Deny
        );
        assert_eq!(
            p.decide(to("10.1.2.3", 443), &client_hello("evil.example"), corp),
            Local::Deny
        );
    }

    #[test]
    fn names_are_read_as_bottled_reads_them() {
        assert_eq!(
            name_in(&client_hello("Example.COM.")).as_deref(),
            Some("example.com")
        );
        assert_eq!(
            name_in(b"GET / HTTP/1.1\r\nHost: a.example:8080\r\nX: y\r\n\r\n").as_deref(),
            Some("a.example")
        );
        assert_eq!(
            name_in(b"GET / HTTP/1.1\r\nHost: [::1]:80\r\n\r\n").as_deref(),
            Some("::1")
        );
        assert_eq!(name_in(b"GET / HTTP/1.1\r\nHost: cut-off"), None); // the last line may be cut off
        assert_eq!(name_in(b"SSH-2.0-OpenSSH\r\n"), None);
        assert_eq!(name_in(&client_hello("x")[..12]), None);
        assert_eq!(name_in(b"\x16\x03\x01\xff\xff\x01\x00"), None);
    }

    #[test]
    fn patterns_match_as_fnmatch_does() {
        assert!(glob("*.example.com", "a.b.example.com"));
        assert!(!glob("*.example.com", "example.com"));
        assert!(glob("GitHub.com", "github.com"));
        assert!(glob("host-?.corp", "host-1.corp"));
        assert!(!glob("github.com", "github.com.evil"));
    }
}
