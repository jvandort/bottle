//! A bottle's way out: the far end of the flows its machine routes here, each one decided by the
//! address the bottle connected to and the name it asked for, which is the bottle's claim, so it's
//! checked before anything's opened: bottle.host's port 80 is its services (host.rs); port 22 of
//! the rest of SSH_NETWORK is sshd in the bottle granted that address (ssh.rs); a credential's host
//! and port, over TLS bottle-cli took off, is its credential's (credentials.rs). Anything else was
//! routed here by a policy out of date, and is refused.

use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Instant;

use super::log::log;
use super::proxy::Header;
use super::{Bottle, Server, credentials, host, ssh};

/// Where a bottle reaches bottle.host (TEST-NET-1's first address), and the bottles it may ssh to:
/// the rest of TEST-NET-1, an address for each.
pub const ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
pub const SSH_PORT: u16 = 22;

fn in_ssh_network(ip: Ipv4Addr) -> bool {
    ip.octets()[..3] == [192, 0, 2]
}

/// Answered as decided; a line in the log for each flow, as folio's Python wrote them: who, where,
/// to what, how it ended, the bytes each way, and how long it took.
pub fn serve(server: &Arc<Server>, bottle: &Arc<Bottle>, header: &Header, stream: UnixStream) {
    let started = Instant::now();
    let target = format!(
        "{}:{}{}",
        header.address,
        header.port,
        header
            .name
            .as_ref()
            .map_or(String::new(), |n| format!(" ({n})"))
    );
    let ip = match header.address {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().unwrap_or(Ipv4Addr::UNSPECIFIED),
    };
    let (to, outcome, status, up, down, target) = if ip == ADDRESS {
        if header.port == 80 {
            let (outcome, status) = host::serve(server, bottle, stream);
            ("host".to_string(), outcome, status, 0, 0, target)
        } else {
            host::refuse(stream, 403, "Forbidden: no such host service");
            ("-".to_string(), "denied".to_string(), None, 0, 0, target)
        }
    } else if in_ssh_network(ip) {
        // Never anywhere on the network: only ever to sshd in the bottle granted there.
        if header.port == SSH_PORT {
            let (outcome, up, down) = ssh::serve(server, bottle, &ip.to_string(), stream);
            ("ssh".to_string(), outcome, None, up, down, target)
        } else {
            host::refuse(stream, 403, "Forbidden: no such bottle service");
            ("-".to_string(), "denied".to_string(), None, 0, 0, target)
        }
    } else {
        // A credential goes only on TLS.
        let injection = match (&header.name, header.tls) {
            (Some(name), true) => bottle
                .injections
                .iter()
                .find(|i| i.matches(name, header.port)),
            _ => None,
        };
        match injection {
            None => (
                "-".to_string(),
                "not-a-service".to_string(),
                None,
                0,
                0,
                target,
            ),
            Some(injection) => {
                let served = credentials::serve(bottle, injection, header, stream);
                let target = if served.injected {
                    format!("{target} +credential")
                } else {
                    target
                };
                (
                    served.to,
                    served.outcome,
                    served.status,
                    served.up,
                    served.down,
                    target,
                )
            }
        }
    };
    let line = format!(
        "{} {target} -> {to} {outcome}{} up={up} down={down} {:.2}s",
        bottle.name,
        status.map_or(String::new(), |s| format!(" {s}")),
        started.elapsed().as_secs_f64()
    );
    log!("bottle.egress", "{line}");
}
