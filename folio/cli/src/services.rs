//! Services: what a machine's policy routes to folio serve.
//!
//! A machine's policy (its network.json, given when bottle makes it: see network()) routes some of
//! its connections to a unix socket of folio serve's: bottle.host's services, ssh to the bottles it
//! may reach, and each credential's host. bottle-cli decides everything else itself, and gives folio
//! serve each routed connection with a PROXY protocol header saying what it was for and which
//! machine it's from (serve/proxy.rs and gateway.rs).

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// Where a bottle reaches bottle.host: its /etc/hosts says so. A documentation address (TEST-NET-1)
/// that nothing real uses, which the bottle's machine routes to folio serve.
pub const HOST_ADDRESS: &str = "192.0.2.1";
pub const HOST: &str = "bottle.host";
/// Where a bottle reaches the bottles it may ssh to (a thread's, its sidecar): the rest of
/// TEST-NET-1, an address for each, under the name its creator gave it.
pub const SSH_NETWORK: &str = "192.0.2.0/24";
pub const SSH_PORT: u16 = 22;
pub const MAX_SSH: usize = 250;
/// Where a bottle's remotes are: http://bottle.host/git/<mount>/<remote>.
pub const GIT_PATH: &str = "/git";

/// The address of a bottle's `index`th ssh grant: 192.0.2.2 onwards.
pub fn ssh_address(index: usize) -> String {
    format!("192.0.2.{}", 2 + index)
}

/// A machine's network policy, as bottle-cli takes it: what it caches, and what it routes to folio
/// serve, by service: bottle.host's (git, messages), ssh to bottles, and each credential's host
/// pattern and port, logged in or not. Its routes say their service, for with_sockets to fill in
/// where it's made.
pub fn network(credentials: &[(String, u16)], caches: &Value) -> Value {
    let mut routes = vec![
        json!({"address": HOST_ADDRESS, "port": 80, "service": "host"}),
        json!({"network": SSH_NETWORK, "port": SSH_PORT, "service": "ssh"}),
    ];
    let mut credentials = credentials.to_vec();
    credentials.sort();
    credentials.dedup();
    routes.extend(
        credentials
            .into_iter()
            .map(|(h, p)| json!({"host": h, "port": p, "service": "credentials"})),
    );
    json!({"allow_private": [], "cache": cache_rules(caches), "routes": routes})
}

/// What a policy says of the hosts whose downloads bottle-cli keeps, as features' cached says them.
pub fn cache_rules(caches: &Value) -> Value {
    Value::Array(
        caches
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                json!({"host": c["host"], "include": c["include"], "exclude": c["exclude"],
                       "ports": c["ports"], "immutable": c["immutable"]})
            })
            .collect(),
    )
}

/// `policy`, each route to its service's socket among `sockets`.
pub fn with_sockets(policy: &Value, sockets: &BTreeMap<String, String>) -> Value {
    let mut policy = policy.clone();
    if let Some(routes) = policy.get_mut("routes").and_then(Value::as_array_mut) {
        for route in routes {
            if let Some(map) = route.as_object_mut()
                && let Some(service) = map.remove("service")
            {
                let socket = sockets.get(service.as_str().unwrap_or_default()).cloned().unwrap_or_default();
                map.insert("socket".into(), socket.into());
            }
        }
    }
    policy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machines_policy_routes_bottle_host_ssh_and_credentials_here_and_says_what_it_caches() {
        let cached = json!([{"host": "deb.example", "include": ["/debian/*.deb"], "exclude": [], "ports": [80],
                             "immutable": true}]);
        let credentials = [("api.example".to_string(), 443), ("ci.example".to_string(), 8111), ("api.example".to_string(), 443)];
        let sockets: BTreeMap<String, String> =
            [("host", "/h.sock"), ("ssh", "/s.sock"), ("credentials", "/c.sock")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let policy = with_sockets(&network(&credentials, &cached), &sockets);
        assert_eq!(
            policy["routes"],
            json!([
                {"address": HOST_ADDRESS, "port": 80, "socket": "/h.sock"},
                {"network": SSH_NETWORK, "port": SSH_PORT, "socket": "/s.sock"},
                {"host": "api.example", "port": 443, "socket": "/c.sock"},
                {"host": "ci.example", "port": 8111, "socket": "/c.sock"},
            ])
        );
        assert_eq!(policy["cache"], cached);
        assert_eq!(policy["allow_private"], json!([]));
        // Made without sockets, each route says its service, for the sockets where it's made.
        assert_eq!(network(&[], &json!([]))["routes"][0]["service"], "host");
    }
}
