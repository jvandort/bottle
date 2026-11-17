//! bottle-cli's machines: bottles' VMs, kept: made once, started and stopped any number of times,
//! their disks kept between. By name, each command's first argument:
//!
//!     bottle-cli create NAME --image IMAGE [--cpus N] [--mem MIB] [--storage GIB]
//!                       [-v SOURCE:TARGET[:ro|:rw]]... [-e NAME=VALUE]... [--user USER]
//!                       [--workdir DIR] [--label NAME=VALUE]... [--network FILE|-]
//!     bottle-cli start NAME [--net-direct]
//!     bottle-cli stop NAME
//!     bottle-cli status NAME [--json]
//!     bottle-cli exec NAME [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--] COMMAND [ARG]...
//!     bottle-cli shell NAME
//!     bottle-cli configure NAME --network FILE|-
//!     bottle-cli rename NAME NEW
//!     bottle-cli delete NAME [-f]
//!     bottle-cli ls [--json]
//!     bottle-cli info [--json]
//!
//! IMAGE is a tag in the store ($BOTTLE_HOME/stages, see build.rs), or a stage's disk
//! (as `image build` prints them, the last): the image is it and the stages below it; its
//! environment is everything's in the machine, under -e's; and what it runs, its entrypoint and
//! command, is started at each start. Mounts are read-only, unless one says :rw. A machine is a
//! directory, $BOTTLE_HOME/krun/machines/NAME: its config, its own disk (sparse, --storage GIB at
//! most), and, while it runs, its pid, its agent's socket and its console's log. Started, it runs
//! in the background, as `bottle-cli boot` ($BOTTLE_BOOT instead, a program taking boot's
//! arguments, to try this without VMs); start returns once its agent answers. Its network is the
//! policy it was made with (--network: kept as its network.json, see bottle-agent-client's policy),
//! every connection decided by it, cached and routed hosts' TLS answered with its own caching CA,
//! made for that policy's hosts as it starts. Without one, with --net-direct, every connection's
//! allowed; otherwise, it has none. A machine is "created" until it's first started, then "running"
//! or "stopped". --user and --workdir are who and where `exec` runs a command unless it says;
//! labels are its creator's, shown by `ls`. `shell` is a login shell, the user's own ($SHELL, as
//! the agent sets it from /etc/passwd), in a terminal; `shell` and `exec` start a stopped machine
//! that has its policy. `configure` gives a stopped machine another policy. `info` is this
//! machine's: its memory and cores, what machines here can have, and its $BOTTLE_HOME.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use bottle_agent_client::policy::Decide as _;
use bottle_agent_client::wait_for_agent;
use bottle_disk::build::Store;
use bottle_formats::listing::{self, State};
use serde::{Deserialize, Serialize};

use crate::FAILED;

mod commands;
mod lifecycle;
mod ls;

pub use commands::*;
use lifecycle::*;
use ls::*;

pub const USAGE: &str = "create NAME --image IMAGE [OPTION]... | start NAME [--net-direct] \
    | stop NAME | status NAME [--json] | exec NAME [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--] COMMAND [ARG]... \
    | shell NAME | configure NAME --network FILE|- | rename NAME NEW | delete NAME [-f] | ls [--json] \
    | info [--json] | cp SRC DST (either NAME:PATH)";

/// How long a stopped machine's VM gets to end, before it's killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// What a machine is made with.
#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// Its stage disks, bottom first.
    image: Vec<PathBuf>,
    cpus: u8,
    mem_mib: u32,
    storage_gib: u64,
    env: Vec<(String, String)>,
    /// Host directories, read-only in it at their targets...
    mounts: Vec<(PathBuf, String)>,
    /// ...but for these targets', writable (-v SOURCE:TARGET:rw).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    writable: Vec<String>,
    /// What its image runs, its entrypoint and command, from its top stage.
    #[serde(default)]
    run: Option<Vec<String>>,
    /// Who `exec` runs a command as, and where, unless it says.
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    /// What its creator says of it, shown by `ls`.
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
}

/// A machine command (see the module's doc): `ls`, or one whose first argument is a machine's name.
pub fn main(command: &str, args: Vec<String>) -> ExitCode {
    if command == "ls" {
        return done(ls(&args));
    }
    let Some((name, rest)) = args.split_first().filter(|(n, _)| !n.starts_with('-')) else {
        return crate::usage_error(&format!("{command} needs a machine's name"));
    };
    let machine = match Machine::new(name) {
        Ok(machine) => machine,
        Err(e) => return crate::usage_error(&e.to_string()),
    };
    done(match command {
        "create" => create(&machine, rest),
        "start" => start(&machine, rest),
        "stop" if rest.is_empty() => machine.stop(),
        "status" => status(&machine, rest),
        "configure" => configure(&machine, rest),
        "rename" => rename(&machine, rest),
        "delete" => delete(&machine, rest),
        "exec" | "shell" => {
            if let Err(e) = machine.up() {
                return done(Err(e));
            }
            if command == "exec" {
                return exec(&machine, rest.to_vec());
            }
            if !rest.is_empty() {
                return crate::usage_error("shell takes a machine's name alone");
            }
            return exec(&machine, LOGIN.map(String::from).to_vec());
        }
        _ => return crate::usage_error(USAGE),
    })
}

/// What `shell` runs: a login shell, the user's own, in a terminal.
pub const LOGIN: [&str; 5] = [
    "-t",
    "--",
    "/bin/sh",
    "-c",
    r#"exec "${SHELL:-/bin/sh}" -l"#,
];

fn done(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// Where machines are: $BOTTLE_HOME/krun/machines.
fn machines() -> PathBuf {
    crate::home().join("krun/machines")
}

/// Whether `name` is a machine there is.
pub fn is_machine(name: &str) -> bool {
    Machine::new(name).is_ok_and(|m| m.exists())
}

/// An image's disks, environment and what it runs (see image_of).
type Image = (Vec<PathBuf>, Vec<(String, String)>, Option<Vec<String>>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain() {
        assert!(Machine::new("bottle-abc_1.2").is_ok());
        for bad in ["", ".hidden", "a/b", "a b", "../x"] {
            assert!(Machine::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_long_directory_gets_a_short_socket() {
        let short = Machine {
            name: "m".into(),
            dir: "/h/.bottle/krun/machines/m".into(),
        };
        assert_eq!(
            short.socket(),
            PathBuf::from("/h/.bottle/krun/machines/m/agent.sock")
        );
        let long = Machine {
            name: "m".into(),
            dir: PathBuf::from("/h").join("x".repeat(120)),
        };
        let socket = long.socket();
        assert!(socket.starts_with("/tmp/"), "{}", socket.display());
        assert!(socket.as_os_str().len() < 100);
    }
}
