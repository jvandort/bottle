//! ssh to the bottles a bottle may reach: a flow to port 22 of an address it was granted is joined
//! to `sshd -i` in the bottle granted there, run by bottle-cli's exec as root, wherever that bottle
//! runs (here, or on a runner, through folio-cli on), until sshd is done. Only ever that: an address
//! granted to nothing, or to a bottle gone or made again since, reaches nothing.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Arc;

use super::{Bottle, Server, exec_command, relay};
use crate::log::log;

const SSHD: &str = "mkdir -p /run/sshd && exec /usr/sbin/sshd -i -e";

/// Joins `stream` to sshd in the bottle `bottle` was granted at `address`: how it ended, and the
/// bytes each way.
pub fn serve(
    server: &Arc<Server>,
    bottle: &Bottle,
    address: &str,
    stream: UnixStream,
) -> (String, u64, u64) {
    let Some(grant) = bottle.ssh.iter().find(|g| g.address == address) else {
        log!(
            "bottle.daemon",
            "{}: no ssh at {address}: {} may ssh to nothing there",
            bottle.name,
            bottle.name
        );
        return ("no-ssh".into(), 0, 0);
    };
    let there = server.bound(&grant.bottle).filter(|b| b.id == grant.id);
    let Some(there) = there else {
        log!(
            "bottle.daemon",
            "{}: no ssh at {address}: bottle {}, which it may ssh to as {}, isn't running",
            bottle.name,
            grant.bottle,
            grant.name
        );
        return ("no-ssh".into(), 0, 0);
    };
    let command = exec_command(&there, &["sh", "-c", SSHD], "root");
    let child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            log!("bottle.daemon", "{}: sshd at {address}: {e}", bottle.name);
            return ("error".into(), 0, 0);
        }
    };
    let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let said = std::thread::spawn(move || {
        let mut said = String::new();
        let _ = stderr.read_to_string(&mut said);
        said
    });
    let (up, down) = match stream.try_clone() {
        Ok(back) => relay(stream, stdin, stdout, back),
        Err(_) => (0, 0),
    };
    let _ = child.kill();
    let status = child.wait().ok();
    let said = said.join().unwrap_or_default();
    if status.is_some_and(|s| !s.success()) && !said.trim().is_empty() {
        log!(
            "bottle.daemon",
            "{}: sshd at {address}: {}",
            bottle.name,
            said.trim().lines().last().unwrap_or_default()
        );
    }
    ("ok".into(), up, down)
}
