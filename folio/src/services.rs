//! Services: what a machine's policy routes to folio serve.
//!
//! A machine's policy (its network.json, given when bottle makes it: see network()) routes some of
//! its connections to a unix socket of folio serve's: bottle.host's services, ssh to the bottles it
//! may reach, and each credential's host. bottle-cli decides everything else itself, and gives folio
//! serve each routed connection with a PROXY protocol header saying what it was for and which
//! machine it's from (serve/proxy.rs and gateway.rs).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};

use bottle_formats::policy::{Cached, Policy, Route};

/// Where a bottle reaches bottle.host: its /etc/hosts says so. A documentation address (TEST-NET-1)
/// that nothing real uses, which the bottle's machine routes to folio serve.
pub const HOST_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
pub const HOST: &str = "bottle.host";
/// Where a bottle reaches the bottles it may ssh to (a thread's, its sidecar): the rest of
/// TEST-NET-1, an address for each, under the name its creator gave it.
pub const SSH_NETWORK: &str = "192.0.2.0/24";
pub const SSH_PORT: u16 = 22;
/// Where a bottle's remotes are: http://bottle.host/git/<mount>/<remote>.
pub const GIT_PATH: &str = "/git";

/// The address of a bottle's `index`th ssh grant: 192.0.2.2 onwards.
pub fn ssh_address(index: usize) -> String {
    format!("192.0.2.{}", 2 + index)
}

/// What folio serve answers for a machine, each on a socket of its own (daemon::services_sockets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// bottle.host's: git, messages.
    Host,
    /// ssh to the bottles it may reach.
    Ssh,
    /// Its credentials' hosts.
    Credentials,
}

impl Service {
    /// Its name among folio serve's sockets.
    pub fn name(self) -> &'static str {
        match self {
            Service::Host => "host",
            Service::Ssh => "ssh",
            Service::Credentials => "credentials",
        }
    }
}

/// A machine's network policy as folio has it: bottle-cli's (Policy), but for its routes, which
/// say the service of folio serve's they go to, for with_sockets to fill in where it's made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub cache: Vec<Cached>,
    pub routes: Vec<(Destination, Service)>,
}

/// Where a route's connections go: to an address, into a network, or to a host by name, on a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    Address(IpAddr, u16),
    Network(String, u16),
    Host(String, u16),
}

/// A machine's network policy: what it caches, and what it routes to folio serve, by service:
/// bottle.host's (git, messages), ssh to bottles, and each credential's host pattern and port,
/// logged in or not.
pub fn network(credentials: &[(String, u16)], cache: Vec<Cached>) -> Network {
    let mut routes = vec![
        (Destination::Address(HOST_ADDRESS.into(), 80), Service::Host),
        (
            Destination::Network(SSH_NETWORK.into(), SSH_PORT),
            Service::Ssh,
        ),
    ];
    let mut credentials = credentials.to_vec();
    credentials.sort();
    credentials.dedup();
    routes.extend(
        credentials
            .into_iter()
            .map(|(h, p)| (Destination::Host(h, p), Service::Credentials)),
    );
    Network { cache, routes }
}

/// `network` as bottle-cli takes it: each route to its service's socket among `sockets`.
pub fn with_sockets(
    network: &Network,
    sockets: &BTreeMap<String, String>,
) -> Result<Policy, String> {
    let routes = network
        .routes
        .iter()
        .map(|(to, service)| {
            let socket = sockets
                .get(service.name())
                .ok_or_else(|| format!("folio serve has no socket for {}", service.name()))?
                .into();
            Ok(match to.clone() {
                Destination::Address(address, port) => Route::Address {
                    address,
                    port,
                    socket,
                },
                Destination::Network(network, port) => Route::Network {
                    network,
                    port,
                    socket,
                },
                Destination::Host(host, port) => Route::Host { host, port, socket },
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(Policy {
        allow_private: vec![],
        cache: network.cache.clone(),
        routes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machines_policy_routes_bottle_host_ssh_and_credentials_here_and_says_what_it_caches() {
        let cached = vec![Cached {
            host: "deb.example".into(),
            include: vec!["/debian/*.deb".into()],
            exclude: vec![],
            ports: vec![80],
            immutable: true,
        }];
        let credentials = [
            ("api.example".to_string(), 443),
            ("ci.example".to_string(), 8111),
            ("api.example".to_string(), 443),
        ];
        let sockets: BTreeMap<String, String> = [
            ("host", "/h.sock"),
            ("ssh", "/s.sock"),
            ("credentials", "/c.sock"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let policy = with_sockets(&network(&credentials, cached.clone()), &sockets).unwrap();
        let route = |json: &str| serde_json::from_str::<Route>(json).unwrap();
        assert_eq!(
            policy.routes,
            [
                route(r#"{"address": "192.0.2.1", "port": 80, "socket": "/h.sock"}"#),
                route(r#"{"network": "192.0.2.0/24", "port": 22, "socket": "/s.sock"}"#),
                route(r#"{"host": "api.example", "port": 443, "socket": "/c.sock"}"#),
                route(r#"{"host": "ci.example", "port": 8111, "socket": "/c.sock"}"#),
            ]
        );
        assert_eq!(policy.cache, cached);
        assert!(policy.allow_private.is_empty());
        // A service folio serve has no socket for is refused, not routed nowhere.
        let some: BTreeMap<String, String> = sockets.into_iter().take(1).collect();
        assert!(with_sockets(&network(&[], vec![]), &some).is_err());
    }
}
