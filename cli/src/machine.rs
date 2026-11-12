//! `bottle-cli machine`: bottles' VMs, kept, as smolvm keeps its machines: made once, started and
//! stopped any number of times, their disks kept between.
//!
//!     bottle-cli machine create --name NAME --image DISK [--cpus N] [--mem MIB] [--storage GIB]
//!                               [-v SOURCE:TARGET[:ro]]... [-e NAME=VALUE]...
//!     bottle-cli machine start --name NAME [--decider ADDRESS:PORT | --net-direct]
//!     bottle-cli machine stop --name NAME
//!     bottle-cli machine delete --name NAME [-f]
//!     bottle-cli machine ls [--json]
//!     bottle-cli machine status --name NAME [--json]
//!     bottle-cli machine exec --name NAME [-i] [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--]
//!                             COMMAND [ARG]...
//!
//! DISK is a stage's disk in a store (as `build` and `import` print them, the last): the image is
//! it and the stages below it, and what it runs, its entrypoint and command, is started at each
//! start, as smolvm starts a container's. Mounts are read-only, whatever they say. A machine is a directory,
//! $BOTTLE_HOME/krun/machines/NAME: its config, its own disk (sparse, --storage GIB at most), and,
//! while it runs, its pid, its agent's socket and its console's log. Started, it runs in the
//! background, as `bottle-cli boot` ($BOTTLE_BOOT instead, a program taking boot's arguments, to
//! try this without VMs); start returns once its agent answers. Its network is boot's options:
//! with --decider, every connection is asked of bottled's decider, as the bottle whose token is in
//! $BOTTLE_DECIDER_TOKEN; with --net-direct, every connection's allowed; without either, it has none. A machine is "created" until it's
//! first started, then "running" or "stopped".

use std::env;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use bottle_disk::build::{Store, wait_for_agent};
use serde::{Deserialize, Serialize};

use crate::FAILED;

pub const USAGE: &str =
    "machine create|start|stop|delete|ls|status|exec ... (see cli/src/machine.rs)";

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
    mounts: Vec<(PathBuf, String)>,
    /// What its image runs, its entrypoint and command, from its top stage.
    #[serde(default)]
    run: Option<Vec<String>>,
}

pub fn main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let command = args.next();
    let args: Vec<String> = args.collect();
    let result = match command.as_deref() {
        Some("create") => create(&args),
        Some("start") => start(&args),
        Some("stop") => named(&args).and_then(|m| m.stop()),
        Some("delete") => delete(&args),
        Some("ls") => ls(&args),
        Some("status") => status(&args),
        Some("exec") => return exec(args),
        _ => {
            return crate::usage_error(
                "machine needs create, start, stop, delete, ls, status or exec",
            );
        }
    };
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
    let home = env::var_os("BOTTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".bottle"));
    home.join("krun/machines")
}

struct Machine {
    name: String,
    dir: PathBuf,
}

/// The machine named by --name in `args`.
fn named(args: &[String]) -> io::Result<Machine> {
    let mut name = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" => name = args.next().cloned(),
            "--json" | "-f" => {}
            other_arg => return Err(other(format!("unknown option {other_arg}"))),
        }
    }
    Machine::new(&name.ok_or_else(|| other("no --name"))?)
}

impl Machine {
    fn new(name: &str) -> io::Result<Machine> {
        let ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            && !name.starts_with('.');
        if !ok {
            return Err(other(format!(
                "{name:?}: not a machine name (letters, digits, . _ -)"
            )));
        }
        Ok(Machine {
            name: name.into(),
            dir: machines().join(name),
        })
    }

    fn exists(&self) -> bool {
        self.dir.join("config.json").exists()
    }

    fn must_exist(&self) -> io::Result<()> {
        if self.exists() {
            Ok(())
        } else {
            Err(other(format!("no machine {}", self.name)))
        }
    }

    fn config(&self) -> io::Result<Config> {
        let path = self.dir.join("config.json");
        let json = fs::read(&path).map_err(|e| other(format!("{}: {e}", path.display())))?;
        serde_json::from_slice(&json).map_err(|e| other(format!("{}: {e}", path.display())))
    }

    fn disk(&self) -> PathBuf {
        self.dir.join("disk.ext4")
    }

    /// Its agent's socket: in its directory, unless that's too long a path for a unix socket (104
    /// bytes, on macOS), then in a directory of this user's in /tmp.
    fn socket(&self) -> PathBuf {
        let here = self.dir.join("agent.sock");
        if here.as_os_str().len() < 100 {
            return here;
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let hash: u64 = self
            .dir
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
            });
        PathBuf::from(format!("/tmp/bottle-krun-{uid}/{hash:016x}.sock"))
    }

    fn pid(&self) -> Option<libc::pid_t> {
        fs::read_to_string(self.dir.join("pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn running(&self) -> bool {
        // SAFETY: kill with signal 0 only asks whether the process is there.
        self.pid()
            .is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }

    /// "created", "running" or "stopped"; None if there's no such machine.
    fn state(&self) -> Option<&'static str> {
        if !self.exists() {
            None
        } else if self.running() {
            Some("running")
        } else if self.dir.join("started").exists() {
            Some("stopped")
        } else {
            Some("created")
        }
    }

    fn start(&self, net: &crate::boot::Net) -> io::Result<()> {
        self.must_exist()?;
        if self.running() {
            return Err(other(format!("{} is running already", self.name)));
        }
        let config = self.config()?;
        let socket = self.socket();
        if let Some(dir) = socket.parent() {
            fs::create_dir_all(dir)?;
            // Only this user's: whoever can reach the socket can run anything in the machine.
            fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        }
        let _ = fs::remove_file(&socket);
        let log = fs::File::create(self.dir.join("boot.log"))?;
        let mut boot = match env::var_os("BOTTLE_BOOT") {
            Some(program) => Command::new(program),
            None => {
                let mut c = Command::new(env::current_exe()?);
                c.arg("boot");
                c
            }
        };
        boot.args(net.args())
            .arg(&socket)
            .arg(self.disk())
            .args(&config.image)
            .args([
                "--cpus",
                &config.cpus.to_string(),
                "--mem",
                &config.mem_mib.to_string(),
            ])
            .args([
                "--size",
                &config.storage_gib.to_string(),
                "--hostname",
                &self.name,
            ])
            .arg("--console")
            .arg(self.dir.join("console.log"));
        for (name, value) in &config.env {
            boot.arg("--env").arg(format!("{name}={value}"));
        }
        for (source, target) in &config.mounts {
            boot.arg("--mount")
                .arg(format!("{}:{target}", source.display()));
        }
        if let Some(run) = &config.run {
            boot.arg("--run").arg(serde_json::to_string(run).unwrap());
        }
        boot.stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // SAFETY: setsid between fork and exec: a session of its own, out of reach of this
        // terminal's signals, and outliving this process.
        unsafe {
            boot.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = boot.spawn()?;
        fs::write(self.dir.join("pid"), child.id().to_string())?;
        let answered = wait_for_agent(&socket, || matches!(child.try_wait(), Ok(None)));
        if let Err(e) = answered {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(self.dir.join("pid"));
            return Err(other(format!(
                "starting {}: {e}\n{}",
                self.name,
                self.logs()
            )));
        }
        fs::write(self.dir.join("started"), "")?;
        Ok(())
    }

    /// The ends of its boot log and console, which say why it failed, if anything does.
    fn logs(&self) -> String {
        let tail = |name: &str| {
            let text = fs::read_to_string(self.dir.join(name)).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(15)..].join("\n")
        };
        format!(
            "boot.log:\n{}\nconsole.log:\n{}",
            tail("boot.log"),
            tail("console.log")
        )
    }

    fn stop(&self) -> io::Result<()> {
        self.must_exist()?;
        let Some(pid) = self.pid().filter(|_| self.running()) else {
            let _ = fs::remove_file(self.dir.join("pid"));
            return Ok(());
        };
        let asked = bottle_agent_client::stop(&self.socket());
        if !self.ends(pid, STOP_TIMEOUT) {
            // SAFETY: kill with a pid.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            self.ends(pid, Duration::from_secs(5));
        }
        let _ = fs::remove_file(self.dir.join("pid"));
        let _ = fs::remove_file(self.socket());
        asked.map_err(|e| other(format!("stopping {}: {e}; killed", self.name)))
    }

    /// Whether process `pid` ends within `timeout`.
    fn ends(&self, pid: libc::pid_t, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        // SAFETY: kill with signal 0 only asks whether the process is there.
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

fn create(args: &[String]) -> io::Result<()> {
    let mut name = None;
    let mut image = None;
    let mut config = Config {
        image: Vec::new(),
        cpus: 2,
        mem_mib: 2048,
        storage_gib: 64,
        env: Vec::new(),
        mounts: Vec::new(),
        run: None,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| other(format!("{arg} needs a value")))
        };
        let number = |v: String| {
            v.parse()
                .map_err(|_| other(format!("{arg} {v}: not a number")))
        };
        match arg.as_str() {
            "--name" => name = Some(value()?),
            "--image" => image = Some(PathBuf::from(value()?)),
            "--cpus" => config.cpus = number(value()?)? as u8,
            "--mem" => config.mem_mib = number(value()?)? as u32,
            "--storage" => config.storage_gib = number(value()?)?,
            "-e" => {
                let pair = value()?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| other(format!("-e {pair}: not NAME=VALUE")))?;
                config.env.push((k.into(), v.into()));
            }
            "-v" => {
                let spec = value()?;
                let spec = spec.strip_suffix(":ro").unwrap_or(&spec);
                let (source, target) = spec
                    .rsplit_once(':')
                    .ok_or_else(|| other(format!("-v {spec}: not SOURCE:TARGET")))?;
                config
                    .mounts
                    .push((fs::canonicalize(source)?, target.into()));
            }
            other_arg => return Err(other(format!("unknown option {other_arg}"))),
        }
    }
    let machine = Machine::new(&name.ok_or_else(|| other("no --name"))?)?;
    let image = image.ok_or_else(|| other("no --image"))?;
    if machine.exists() {
        return Err(other(format!("there's a machine {} already", machine.name)));
    }
    (config.image, config.run) = image_of(&image)?;
    let mke2fs = crate::mke2fs()?;
    fs::create_dir_all(&machine.dir)?;
    let made = (|| {
        bottle_disk::mke2fs::empty(&mke2fs, &machine.disk(), config.storage_gib << 30)?;
        fs::write(
            machine.dir.join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
    })();
    if made.is_err() {
        let _ = fs::remove_dir_all(&machine.dir);
    }
    made
}

/// The disks of the image whose top stage's disk is `disk`, bottom first, and what it runs (its
/// entrypoint, then its command): its store says, if it's in one; else it's an image of one stage
/// that runs nothing.
fn image_of(disk: &Path) -> io::Result<(Vec<PathBuf>, Option<Vec<String>>)> {
    let disk = fs::canonicalize(disk).map_err(|e| other(format!("{}: {e}", disk.display())))?;
    let (Some(dir), Some(name)) = (disk.parent(), disk.file_stem().and_then(|s| s.to_str())) else {
        return Ok((vec![disk], None));
    };
    if !dir.join(format!("{name}.json")).exists() {
        return Ok((vec![disk], None));
    }
    let store = Store {
        dir: dir.to_path_buf(),
        mke2fs: PathBuf::new(),
    };
    let stage = store.stage(name)?;
    let run: Vec<String> = stage
        .entrypoint
        .into_iter()
        .flatten()
        .chain(stage.cmd.into_iter().flatten())
        .collect();
    Ok((store.disks(name)?, Some(run).filter(|r| !r.is_empty())))
}

/// `machine start --name NAME [--decider ADDRESS:PORT | --net-direct]`: the network is the start's,
/// as a decider and token are a launch's (the token in $BOTTLE_DECIDER_TOKEN, which boot gets).
fn start(args: &[String]) -> io::Result<()> {
    let mut net = crate::boot::Net::None;
    let mut rest = Vec::new();
    let mut given = args.iter();
    while let Some(arg) = given.next() {
        match net.take(arg, || given.next().cloned()) {
            Some(taken) => taken.map_err(other)?,
            None => rest.push(arg.clone()),
        }
    }
    named(&rest)?.start(&net)
}

/// The disks of every machine's image, running or not: what `gc` keeps.
pub fn disks_in_use() -> io::Result<Vec<PathBuf>> {
    let mut disks = Vec::new();
    let Ok(entries) = fs::read_dir(machines()) else {
        return Ok(disks);
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        if let Ok(machine) = Machine::new(&name)
            && machine.exists()
        {
            disks.extend(machine.config()?.image);
        }
    }
    Ok(disks)
}

fn delete(args: &[String]) -> io::Result<()> {
    let machine = named(args)?;
    machine.must_exist()?;
    if args.iter().any(|a| a == "-f") {
        if let Some(pid) = machine.pid().filter(|_| machine.running()) {
            // SAFETY: kill with a pid.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            machine.ends(pid, Duration::from_secs(5));
        }
    } else {
        machine.stop()?;
    }
    let _ = fs::remove_file(machine.socket());
    fs::remove_dir_all(&machine.dir)
}

fn ls(args: &[String]) -> io::Result<()> {
    let json = args.iter().any(|a| a == "--json");
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(machines()) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if let Ok(machine) = Machine::new(&name)
                && let Some(state) = machine.state()
            {
                found.push((name, state));
            }
        }
    }
    found.sort();
    if json {
        let list: Vec<_> = found
            .iter()
            .map(|(name, state)| serde_json::json!({"name": name, "state": state}))
            .collect();
        println!("{}", serde_json::Value::Array(list));
    } else {
        for (name, state) in found {
            println!("{name} {state}");
        }
    }
    Ok(())
}

fn status(args: &[String]) -> io::Result<()> {
    let machine = named(args)?;
    let state = machine
        .state()
        .ok_or_else(|| other(format!("no machine {}", machine.name)))?;
    if args.iter().any(|a| a == "--json") {
        println!(
            "{}",
            serde_json::json!({"name": machine.name, "state": state})
        );
    } else {
        println!("{state}");
    }
    Ok(())
}

/// `machine exec`: `exec` on the machine's socket. -i is taken and ignored, stdin being always
/// passed on.
fn exec(args: Vec<String>) -> ExitCode {
    let mut name = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" => name = args.next(),
            "-i" => {}
            "--" => {
                rest.push(arg);
                rest.extend(args.by_ref());
            }
            _ => rest.push(arg),
        }
    }
    let machine = match name
        .ok_or_else(|| other("no --name"))
        .and_then(|n| Machine::new(&n))
    {
        Ok(machine) => machine,
        Err(e) => return crate::usage_error(&e.to_string()),
    };
    if !machine.running() {
        eprintln!("bottle-cli: {} isn't running", machine.name);
        return ExitCode::from(FAILED);
    }
    let socket = machine.socket().to_string_lossy().into_owned();
    crate::exec::main(std::iter::once(socket).chain(rest))
}

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
