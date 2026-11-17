//! A machine's network policy (`bottle-cli create --network FILE`, kept as its network.json): which
//! hosts' downloads are kept on the host, which connections a service on the host answers (routes,
//! each to a unix socket), and the private addresses the bottle may reach by name.

use std::net::IpAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Names (globs) by which the bottle may reach a private address.
    #[serde(default)]
    pub allow_private: Vec<String>,
    /// The hosts whose downloads are kept on the host.
    #[serde(default)]
    pub cache: Vec<Cached>,
    /// The connections a service answers, on its unix socket.
    #[serde(default)]
    pub routes: Vec<Route>,
}

/// Connections a service answers: to an address, into a network, or by name, each on a port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Route {
    Address {
        address: IpAddr,
        port: u16,
        socket: PathBuf,
    },
    Network {
        /// CIDR: `10.0.0.0/8`.
        network: String,
        port: u16,
        socket: PathBuf,
    },
    Host {
        /// A name, or a glob of names.
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

    pub fn port(&self) -> u16 {
        match self {
            Route::Address { port, .. }
            | Route::Network { port, .. }
            | Route::Host { port, .. } => *port,
        }
    }
}

/// A host (a name, or a glob of names) whose downloads are kept, asked on `ports`: those at paths
/// matching `include`, if it says any, but for paths matching `exclude`; `immutable` if what's at
/// a path never changes, whatever the host says about keeping it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Where a cached host is asked: over https, whose TLS is terminated on the host, or plain http.
pub const HTTPS_PORT: u16 = 443;
pub const HTTP_PORT: u16 = 80;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_with_a_key_it_doesnt_know_is_refused() {
        let read = |json: &str| serde_json::from_str::<Policy>(json);
        let policy = read(
            r#"{"cache": [{"host": "deb.example"}],
                "routes": [{"host": "api.example", "port": 443, "socket": "/c.sock"},
                           {"address": "10.0.0.1", "port": 80, "socket": "/h.sock"}]}"#,
        )
        .unwrap();
        assert_eq!(policy.cache[0].ports, [HTTPS_PORT]);
        assert_eq!(policy.routes[1].port(), 80);
        assert!(read(r#"{"allow_privat": []}"#).is_err());
        assert!(read(r#"{"cache": [{"host": "x", "immutible": true}]}"#).is_err());
        assert!(
            read(r#"{"routes": [{"host": "x", "port": 1, "socket": "/s", "service": "s"}]}"#)
                .is_err()
        );
        // What it writes, it reads.
        let written = serde_json::to_string(&policy).unwrap();
        assert_eq!(read(&written).unwrap(), policy);
    }
}
