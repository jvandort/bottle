//! `bottle-cli build` and `bottle-cli import`: stages made in VMs, in a store (see bottle-disk's
//! build): each prints the disks of the image it made, bottom first, as `boot` takes them.
//!
//!     bottle-cli build RECIPE STORE [--decider ADDRESS:PORT | --net-direct]
//!     bottle-cli import ARCHIVE STORE [--decider ADDRESS:PORT | --net-direct]
//!
//! RECIPE is a JSON file, steps run on the stage it's `from` (see bottle_disk::build::Recipe);
//! ARCHIVE an OCI layout archive of one image. Each VM is `bottle-cli boot`, this program again,
//! its console kept for when it fails; libkrun and the rest are found as boot finds them, and the
//! network options are boot's: a build's VM has the network they say.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use bottle_disk::Archive;
use bottle_disk::build::{Launch, Mount, Recipe, Store, Vm, wait_for_agent};

use crate::FAILED;
use crate::boot::Net;

pub const BUILD_USAGE: &str = "build RECIPE STORE [--decider ADDRESS:PORT | --net-direct]";
pub const IMPORT_USAGE: &str = "import ARCHIVE STORE [--decider ADDRESS:PORT | --net-direct]";

/// A build VM's size: builds compile things.
const CPUS: &str = "4";
const MEM_MIB: &str = "4096";

/// Two arguments, and the network options (see boot's Net), in any order.
fn two_and_net(
    args: impl Iterator<Item = String>,
    what: &str,
) -> Result<(String, String, Net), ExitCode> {
    let mut positional = Vec::new();
    let mut net = Net::None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match net.take(&arg, || args.next()) {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(crate::usage_error(&e)),
            None => positional.push(arg),
        }
    }
    match <[String; 2]>::try_from(positional) {
        Ok([a, b]) => Ok((a, b, net)),
        Err(_) => Err(crate::usage_error(what)),
    }
}

pub fn build(args: impl Iterator<Item = String>) -> ExitCode {
    let (recipe, store, net) = match two_and_net(args, "build needs RECIPE and STORE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(Path::new(&store))?;
        let json = fs::read(&recipe).map_err(|e| io::Error::other(format!("{recipe}: {e}")))?;
        let recipe: Recipe = serde_json::from_slice(&json)
            .map_err(|e| io::Error::other(format!("{recipe}: {e}")))?;
        let name = store.build(&recipe, &Boot { net })?;
        store.disks(&name)
    })())
}

pub fn import(args: impl Iterator<Item = String>) -> ExitCode {
    let (archive, store, net) = match two_and_net(args, "import needs ARCHIVE and STORE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(Path::new(&store))?;
        let archive = Archive::open(Path::new(&archive))?;
        let image = archive.image()?;
        let name = store.import(&archive, &image, &Boot { net })?;
        store.disks(&name)
    })())
}

fn store_at(dir: &Path) -> io::Result<Store> {
    fs::create_dir_all(dir)?;
    Ok(Store {
        dir: dir.to_path_buf(),
        mke2fs: crate::mke2fs()?,
    })
}

fn report(disks: io::Result<Vec<PathBuf>>) -> ExitCode {
    match disks {
        Ok(disks) => {
            for disk in disks {
                println!("{}", disk.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

/// Boots build VMs with `bottle-cli boot`.
struct Boot {
    net: Net,
}

struct Booted {
    child: Child,
    socket: PathBuf,
    console: PathBuf,
}

impl Launch for Boot {
    fn launch(
        &self,
        lowers: &[PathBuf],
        upper: &Path,
        mount: Option<&Mount>,
    ) -> io::Result<Box<dyn Vm>> {
        // A unix socket's path has room for only so many characters (104 on macOS): in the
        // temporary directory, not the store.
        static N: AtomicU32 = AtomicU32::new(0);
        let base = env::temp_dir().join(format!(
            "bottle-build-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let socket = base.with_extension("sock");
        let console = base.with_extension("console");
        let child = Command::new(env::current_exe()?)
            .arg("boot")
            .arg(&socket)
            .arg(upper)
            .args(lowers)
            .args(self.net.args())
            .args(mount.into_iter().flat_map(|m| {
                [
                    "--mount".to_string(),
                    format!("{}:{}", m.source.display(), m.target),
                ]
            }))
            .args(["--cpus", CPUS, "--mem", MEM_MIB, "--console"])
            .arg(&console)
            .stdin(Stdio::null())
            .spawn()?;
        let mut booted = Booted {
            child,
            socket,
            console,
        };
        let socket = booted.socket.clone();
        // Dropped if it doesn't answer: killed.
        wait_for_agent(&socket, || matches!(booted.child.try_wait(), Ok(None)))
            .map_err(|e| booted.with_console(e))?;
        Ok(Box::new(booted))
    }
}

impl Booted {
    /// `e`, and the end of the VM's console, which says why, if anything does.
    fn with_console(&self, e: io::Error) -> io::Error {
        let console = fs::read_to_string(&self.console).unwrap_or_default();
        let tail: Vec<&str> = console.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        io::Error::other(format!(
            "{e}\nthe VM's console, at the end:\n{}",
            tail.join("\n")
        ))
    }
}

impl Vm for Booted {
    fn socket(&self) -> &Path {
        &self.socket
    }

    fn wait(mut self: Box<Self>) -> io::Result<()> {
        let status = self.child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(self.with_console(io::Error::other(format!("the VM ended so: {status}"))))
        }
    }
}

impl Drop for Booted {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.console);
    }
}
