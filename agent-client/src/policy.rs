//! What the host's side of a bottle's network decides about each connection the bottle makes, by
//! its machine's network policy (its network.json, given when it's made: see bottle/services.py's
//! network): which hosts' downloads are kept here (cache.rs), which connections a service answers
//! (routes, each to a unix socket: see net.rs), and the private addresses the bottle may reach by
//! name.
//!
//! A connection is routed when it goes to an address or network a route names, or when its first
//! bytes give a name a route names on that port: a TLS ClientHello's server name, or an HTTP
//! request's Host; it's cached when its name is a cached host's. One whose name can't be told yet
//! (nothing sent, or part of a TLS record) is read further first when a name may decide it on its
//! port. Otherwise: loopback, link-local, multicast and unspecified addresses are refused; public
//! ones are connected; a private one only by a name `allow_private` matches, that leads there.
//! IPv6 is refused: the bottle has no route for it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub allow_private: Vec<String>,
    /// The hosts whose downloads are kept here (see cache.rs), as bottled's cache.Cached has them.
    #[serde(default)]
    pub cache: Vec<Cached>,
    /// The connections a service answers, on its unix socket.
    #[serde(default)]
    pub routes: Vec<Route>,
}

/// Connections a service answers: to an address, into a network, or by name, each on a port.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Route {
    Address {
        address: IpAddr,
        port: u16,
        socket: PathBuf,
    },
    Network {
        network: String,
        port: u16,
        socket: PathBuf,
    },
    Host {
        host: String,
        port: u16,
        socket: PathBuf,
    },
}

impl Route {
    pub fn socket(&self) -> &PathBuf {
        match self {
            Route::Address { socket, .. }
            | Route::Network { socket, .. }
            | Route::Host { socket, .. } => socket,
        }
    }

    fn port(&self) -> u16 {
        match self {
            Route::Address { port, .. }
            | Route::Network { port, .. }
            | Route::Host { port, .. } => *port,
        }
    }
}

/// A host whose downloads are kept, asked on `ports`: those at paths matching `include`, if it
/// says any, but for paths matching `exclude`; `immutable` if what's at a path never changes,
/// whatever the host says about keeping it.
#[derive(Clone, Debug, Deserialize)]
pub struct Cached {
    pub host: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default = "https_port")]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub immutable: bool,
}

fn https_port() -> Vec<u16> {
    vec![HTTPS_PORT]
}

/// Where a cached host is asked: over https, whose TLS is terminated here, or plain http.
pub const HTTPS_PORT: u16 = 443;
pub const HTTP_PORT: u16 = 80;

impl Cached {
    pub fn matches(&self, host: &str) -> bool {
        glob(&self.host, host)
    }

    /// Whether a request for `path` isn't one to keep.
    pub fn excludes(&self, path: &str) -> bool {
        (!self.include.is_empty() && !self.include.iter().any(|p| glob_case(p, path)))
            || self.exclude.iter().any(|p| glob_case(p, path))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Local {
    Direct,
    Deny,
    /// Answered here, from the store where it can be: the policy's cache entry for its host.
    Cache(usize),
    /// The policy's route for it: to its service's socket.
    Route(usize),
    /// Its name may decide it, and isn't all here yet: decided again once it's read, with `opened`.
    Unread,
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
        self.decide_opened(to, first, resolve, false)
    }

    /// As decide; `opened`: as much of its opening as will be read has been, so it's never Unread.
    pub fn decide_opened(
        &self,
        to: SocketAddr,
        first: &[u8],
        resolve: impl Fn(&str) -> Vec<IpAddr>,
        opened: bool,
    ) -> Local {
        let ip = match to.ip() {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => return Local::Deny,
            },
            v4 => v4,
        };
        let port = to.port();
        for (i, route) in self.routes.iter().enumerate() {
            match route {
                Route::Address {
                    address, port: p, ..
                } if *address == ip && *p == port => {
                    return Local::Route(i);
                }
                Route::Network {
                    network, port: p, ..
                } if *p == port && in_network(ip, network) => {
                    return Local::Route(i);
                }
                _ => {}
            }
        }
        let name = name_in(first);
        // Over https, the name a ClientHello claims; over plain http, an HTTP request's Host.
        let tls = first.first() == Some(&0x16);
        match &name {
            Some(name) => {
                if let Some(i) = self.routes.iter().position(
                    |r| matches!(r, Route::Host { host, port: p, .. } if *p == port && glob(host, name)),
                ) {
                    return Local::Route(i);
                }
                if let Some(i) = self.cache.iter().position(|c| {
                    c.ports.contains(&port) && tls == (port != HTTP_PORT) && c.matches(name)
                }) {
                    return Local::Cache(i);
                }
            }
            None if !opened
                && opening_unread(first)
                && (self
                    .routes
                    .iter()
                    .any(|r| matches!(r, Route::Host { .. }) && r.port() == port)
                    || self.cache.iter().any(|c| c.ports.contains(&port))) =>
            {
                return Local::Unread;
            }
            None => {}
        }
        let IpAddr::V4(v4) = ip else {
            return Local::Deny;
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

impl Policy {
    /// The hosts whose TLS is terminated here: those cached over https, and those routed by name.
    /// What a machine's caching CA is for.
    pub fn terminated_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .cache
            .iter()
            .filter(|c| c.ports.iter().any(|&p| p != HTTP_PORT))
            .map(|c| c.host.to_lowercase())
            .chain(self.routes.iter().filter_map(|r| match r {
                Route::Host { host, .. } => Some(host.to_lowercase()),
                _ => None,
            }))
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// The addresses of `name` a connection may be made to from here, for a cached host's
    /// fetches: public ones, and private ones too if the name's allowed private.
    pub fn reachable(&self, name: &str, resolve: impl Fn(&str) -> Vec<IpAddr>) -> Vec<IpAddr> {
        let private = self.allow_private.iter().any(|p| glob(p, name));
        resolve(name)
            .into_iter()
            .filter(|ip| match ip {
                IpAddr::V4(v4) => {
                    is_global(*v4)
                        || (private
                            && !(v4.is_loopback()
                                || v4.is_link_local()
                                || v4.is_multicast()
                                || v4.is_unspecified()))
                }
                IpAddr::V6(_) => false,
            })
            .collect()
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
    glob_case(&pattern.to_lowercase(), &name.to_lowercase())
}

/// Whether `text` matches `pattern`, as fnmatchcase does: `*` any run of characters, `?` one.
fn glob_case(pattern: &str, text: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
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

    const POLICY: &str = r#"{"allow_private": ["*.corp.example"], "routes": [
        {"address": "192.0.2.1", "port": 80, "socket": "/s"},
        {"host": "github.com", "port": 443, "socket": "/s"}]}"#;

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
        assert_eq!(p.decide(to("::1", 443), b"", nothing), Local::Deny); // IPv6: no route for it
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
    fn a_cached_hosts_connections_are_answered_here_unless_theyre_routed() {
        let p = policy(
            r#"{"routes": [{"host": "both.example", "port": 443, "socket": "/s"}], "cache": [
                {"host": "repo.maven.example", "exclude": ["*/maven-metadata.xml*"]},
                {"host": "both.example"},
                {"host": "deb.debian.example", "include": ["/debian/*.deb"], "ports": [80], "immutable": true}]}"#,
        );
        let nothing = |_: &str| vec![];
        let get = |host: &str| format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes();
        let maven = client_hello("repo.maven.example");
        assert_eq!(
            p.decide(to("1.2.3.4", 443), &maven, nothing),
            Local::Cache(0)
        );
        assert_eq!(
            p.decide(to("1.2.3.4", 80), &get("deb.debian.example"), nothing),
            Local::Cache(2)
        );
        // A routed host is its service's, cached or not.
        assert_eq!(
            p.decide(to("1.2.3.4", 443), &client_hello("both.example"), nothing),
            Local::Route(0)
        );
        // Plain http on the https port, or TLS on the plain one, isn't the cache's.
        assert_eq!(
            p.decide(to("1.2.3.4", 443), &get("repo.maven.example"), nothing),
            Local::Direct
        );
        assert_eq!(
            p.decide(
                to("1.2.3.4", 80),
                &client_hello("deb.debian.example"),
                nothing
            ),
            Local::Direct
        );
        let (maven, deb) = (&p.cache[0], &p.cache[2]);
        assert!(maven.excludes("/org/x/maven-metadata.xml.sha1"));
        assert!(!maven.excludes("/org/x/1.0/x-1.0.jar"));
        assert!(!deb.excludes("/debian/pool/main/h/hello/hello_2.10-5_amd64.deb"));
        assert!(deb.excludes("/debian/dists/trixie/InRelease"));
        assert!(deb.excludes("/DEBIAN/pool/x.deb")); // paths match with case
        assert_eq!(maven.ports, [443]);
    }

    #[test]
    fn a_routed_connection_goes_to_its_service_by_address_network_or_name() {
        let p = policy(
            r#"{"routes": [
                {"address": "192.0.2.1", "port": 80, "socket": "/s/host.sock"},
                {"network": "192.0.2.0/24", "port": 22, "socket": "/s/ssh.sock"},
                {"host": "*.anthropic.example", "port": 443, "socket": "/s/folio.sock"}],
              "cache": [{"host": "deb.example", "ports": [80]}, {"host": "repo.example"}]}"#,
        );
        let nothing = |_: &str| vec![];
        assert_eq!(p.decide(to("192.0.2.1", 80), b"", nothing), Local::Route(0));
        assert_eq!(p.decide(to("192.0.2.9", 22), b"", nothing), Local::Route(1));
        assert_eq!(
            p.decide(
                to("1.2.3.4", 443),
                &client_hello("api.anthropic.example"),
                nothing
            ),
            Local::Route(2)
        );
        assert_eq!(p.routes[2].socket(), &PathBuf::from("/s/folio.sock"));
        // Another name, or none on another port: as any connection.
        assert_eq!(
            p.decide(to("1.2.3.4", 443), &client_hello("other.example"), nothing),
            Local::Direct
        );
        assert_eq!(p.decide(to("1.2.3.4", 8080), b"", nothing), Local::Direct);
        // A name that may decide it, not here yet: read it first, then decide as it stands.
        let hello = client_hello("api.anthropic.example");
        assert_eq!(p.decide(to("1.2.3.4", 443), b"", nothing), Local::Unread);
        assert_eq!(
            p.decide(to("1.2.3.4", 443), &hello[..20], nothing),
            Local::Unread
        );
        assert_eq!(
            p.decide_opened(to("1.2.3.4", 443), b"", nothing, true),
            Local::Direct
        );
        // What a machine's caching CA is for: the hosts cached over https, and those routed by name.
        assert_eq!(
            p.terminated_hosts(),
            ["*.anthropic.example", "repo.example"]
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
