//! `folio-cli runner`: the other machines folio makes bottles on, over ssh.
//!
//! A runner is a Linux machine with /dev/kvm, reached as the user's own ssh reaches it: a
//! DESTINATION from ~/.ssh/config, or user@host, with their keys and agent. ssh runs in batch mode,
//! never asking for a password or a host key: a runner it can't reach without asking is unreachable.
//! Each runner gets one shared connection (ssh's ControlMaster), kept for a while after its last
//! use. A runner has bottle-cli and nothing else of bottle's, in ~/.bottle/dist there: the
//! distribution for its architecture of the bottle-cli folio runs here, copied over whenever it
//! differs, file by file (ensure_dist), before anything's asked of it. That's bottle-cli's own, if
//! it's built for the runner's target, else the one beside it (`dist-TARGET`; in
//! $FOLIO_RUNNER_DISTS instead, if it's set): every distribution is built with bottle-cli, so none
//! is built for a runner when it's used.
//!
//! `on NAME COMMAND...` is bottle-cli's COMMAND there, its own machines by their own names: its
//! output, exit status and (for `exec -t` and `shell`) terminal this process's; `--connect-timeout
//! SECONDS` before NAME waits that long for it to answer, not 10 (a list asks briefly). Paths in its
//! arguments are the runner's, but for `image build`'s recipe, what it mounts and its --network
//! file, which are this machine's, sent with it. `runner ssh` is the shared connection, for a
//! command there: what git's GIT_SSH_COMMAND can be, with a URL `NAME:PATH` and
//! GIT_SSH_VARIANT=simple. `runner check` says what a machine (a runner's, or one to add) is and
//! what it lacks, and adds nothing.
//!
//! `runner forward NAME THERE=HERE...` makes each unix socket HERE, on this machine, reachable
//! there at THERE, over an ssh connection of its own (-R), for as long as it runs: connected again
//! whenever ssh drops, waiting longer each time, up to a minute. It ends when it's told to
//! (SIGTERM, its ssh with it: it's that process group's leader, as folio serve starts it) or when
//! whoever started it has gone. folio serve runs one per runner its bottles are on, for its
//! services' sockets.
//!
//! `runner detach` is for turning a runner off: it pauses the threads running there, so their
//! snapshots are here and they can resume anywhere, and stops its other running machines (asking
//! first, on a terminal; told no, or with no one to ask, it touches nothing and stays attached), and
//! marks it detached; `--offline` marks one that's off already, unasked. A detached runner isn't asked
//! anything. Using one again (`on`, `runner ssh`) attaches it again if it answers, and says it's
//! off if it doesn't; `runner attach` does that alone, and names the machines detaching it stopped.
//! `runner rm` forgets one, touching nothing there. `runner home` says its user's home there, which
//! paths there (a forward's sockets, say) are under: asked once, and kept with it.
//!
//! Runners are listed in folio's home, runners.json, `{"version": 1, "runners": {NAME: {"ssh":
//! DESTINATION, "home": PATH, "detached": {"at": SECONDS, "stopped": [MACHINE]...}}}}`; what else an
//! entry says is kept as it is.

use ssh::Runner;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod commands;
mod dist;
mod forward;
mod on;
mod ssh;

pub use commands::main;
pub use forward::forward_until;
pub use on::on;

const USAGE: &str = "runner add NAME --ssh DESTINATION | runner ls [--json] | runner rm NAME \
                     | runner ssh NAME [COMMAND]... | runner check NAME|DESTINATION [--json] \
                     | runner detach NAME [--offline] | runner attach NAME \
                     | runner home NAME | runner forward NAME THERE=HERE... \
                     | on [--connect-timeout SECONDS] NAME COMMAND [ARG]...";

/// ssh's own failures, connecting or authenticating, exit 255.
const SSH_FAILED: i32 = 255;
/// How long a runner's shared connection outlives its last use, in seconds.
const PERSIST: u32 = 600;
/// How long to wait for a runner to answer at all, in seconds: one that's off is said to be, soon.
const CONNECT_TIMEOUT: u32 = 10;
/// macOS caps unix socket paths at 104 bytes; a control socket's is ssh's to extend, by up to 17.
const MAX_CONTROL_PATH: usize = 80;
/// Where a runner has bottle-cli, under its home.
const DIST: &str = ".bottle/dist";
/// Beside a distribution's files, as bottle's build writes it: each one's sha256 and name, as
/// sha256sum writes them.
const MANIFEST: &str = ".files";
/// The longest a forward waits to connect again.
const MOST_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    version: u32,
    runners: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    ssh: String,
    /// What else is said of it (that it's detached, say).
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

impl Entry {
    fn detached(&self) -> bool {
        self.rest.contains_key("detached")
    }
}

fn registry_path() -> PathBuf {
    crate::home().join("runners.json")
}

fn registry() -> io::Result<Registry> {
    let path = registry_path();
    let json = match fs::read(&path) {
        Ok(json) => json,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Registry {
                version: 1,
                ..Default::default()
            });
        }
        Err(e) => return Err(e),
    };
    serde_json::from_slice(&json).map_err(|e| other(format!("{}: {e}", path.display())))
}

fn write_registry(registry: &Registry) -> io::Result<()> {
    let path = registry_path();
    fs::create_dir_all(path.parent().unwrap())?;
    let staged = path.with_extension("json.tmp");
    fs::write(&staged, serde_json::to_vec_pretty(registry).unwrap())?;
    fs::rename(staged, path)
}

/// Every runner added, by name.
fn registered() -> io::Result<BTreeMap<String, Entry>> {
    Ok(registry()?.runners)
}

/// Every runner added, by name: whether it's detached.
pub fn detached() -> io::Result<BTreeMap<String, bool>> {
    Ok(registered()?
        .into_iter()
        .map(|(name, entry)| {
            let detached = entry.detached();
            (name, detached)
        })
        .collect())
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// Runner `name`'s user's home there, asked once and kept in its entry.
pub fn home(name: &str) -> io::Result<String> {
    let entry = registered()?
        .remove(name)
        .ok_or_else(|| other(format!("no runner named {name}")))?;
    if let Some(home) = entry.rest.get("home").and_then(|h| h.as_str()) {
        return Ok(home.to_string());
    }
    let runner = Runner::named(name)?;
    let output = runner.run(r#"printf %s "$HOME""#, None)?;
    let home = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() || !home.starts_with('/') {
        return Err(other(format!(
            "runner {name}: no home ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut registry = registry()?;
    if let Some(entry) = registry.runners.get_mut(name) {
        entry.rest.insert("home".into(), home.clone().into());
    }
    write_registry(&registry)?;
    Ok(home)
}
