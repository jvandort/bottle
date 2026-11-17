//! A runner's forwards: unix sockets here, reachable there, over an ssh of their own, kept up.

use std::io::Read;

use std::process::Stdio;
use std::time::{Duration, Instant};
use std::{io, thread};

use super::ssh::Runner;
use super::{MOST_BACKOFF, other};

/// `runner forward NAME THERE=HERE...` (see the module's doc).
pub(super) fn forward(name: &str, forwards: &[String]) -> io::Result<()> {
    let parent = rustix::process::getppid();
    forward_until(name, forwards, &|| rustix::process::getppid() != parent)
}

/// Forwards `forwards` (THERE=HERE) to runner `name` until `gone` says to stop: `runner forward`'s
/// loop, which folio serve runs in a thread of its own for each runner it forwards to.
pub fn forward_until(name: &str, forwards: &[String], gone: &dyn Fn() -> bool) -> io::Result<()> {
    let runner = Runner::named(name)?;
    let pairs: Vec<(&str, &str)> = forwards
        .iter()
        .map(|f| {
            f.split_once('=')
                .ok_or_else(|| other(format!("{f}: not THERE=HERE")))
        })
        .collect::<io::Result<_>>()?;
    let mut backoff = Duration::from_secs(1);
    while !gone() {
        let since = Instant::now();
        match connect(&runner, &pairs, gone) {
            Ok(None) => return Ok(()),
            Ok(Some(why)) => eprintln!("folio: forward to {name} dropped: {why}"),
            Err(e) => eprintln!("folio: forward to {name}: {e}"),
        }
        // One that held a while starts over: a drop isn't a failure to connect.
        if since.elapsed() > MOST_BACKOFF {
            backoff = Duration::from_secs(1);
        }
        let waited = Instant::now();
        while waited.elapsed() < backoff && !gone() {
            thread::sleep(Duration::from_millis(200));
        }
        backoff = (backoff * 2).min(MOST_BACKOFF);
    }
    Ok(())
}

/// One ssh forwarding `pairs`, until it drops (Some, why) or whoever started this has gone (None).
pub(super) fn connect(
    runner: &Runner,
    pairs: &[(&str, &str)],
    gone: &dyn Fn() -> bool,
) -> io::Result<Option<String>> {
    // Whatever a forward that died left there, gone first: ssh won't bind over it.
    let theres: Vec<String> = pairs.iter().map(|(t, _)| crate::shell::quote(t)).collect();
    let dirs: Vec<String> = pairs
        .iter()
        .filter_map(|(t, _)| t.rsplit_once('/').map(|(d, _)| crate::shell::quote(d)))
        .collect();
    let made = runner.run(
        &format!("mkdir -p {} && rm -f {}", dirs.join(" "), theres.join(" ")),
        None,
    )?;
    if !made.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&made.stderr).trim().to_string(),
        ));
    }
    let mut ssh = runner.bare_ssh(
        &[
            "ExitOnForwardFailure=yes",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            // A connection of its own, not the shared one: it lives as long as the forward does.
            "ControlMaster=no",
            "ControlPath=none",
        ]
        .map(String::from),
    );
    ssh.args(["-N", "-T"]);
    for (there, here) in pairs {
        ssh.args(["-R", &format!("{there}:{here}")]);
    }
    ssh.args(["--", &runner.destination])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = ssh.spawn()?;
    eprintln!("folio: forwarding to {}", runner.name);
    loop {
        if let Some(status) = child.try_wait()? {
            let mut said = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut said);
            }
            let why = said
                .lines()
                .last()
                .map_or(format!("ssh exited: {status}"), String::from);
            return Ok(Some(why));
        }
        if gone() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(200));
    }
}
