//! bottle-cli's machines: bottles' VMs, kept: made once, started and stopped any number of times,
//! their disks kept between. By name, each command's first argument:
//!
//!     bottle-cli create NAME --image IMAGE [--store PATH] [--cpus N] [--mem MIB] [--storage GIB]
//!                       [-v SOURCE:TARGET[:ro]]... [-e NAME=VALUE]... [--user USER]
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
//! IMAGE is a tag in a store (its default, $BOTTLE_HOME/stages, or --store's), or a stage's disk
//! (as `image build` prints them, the last): the image is it and the stages below it; its
//! environment is everything's in the machine, under -e's; and what it runs, its entrypoint and
//! command, is started at each start. Mounts are read-only, whatever they say. A machine is a
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
//!
//! A runner's machine is RUNNER/NAME, every command the same (see remote.rs).

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

pub const USAGE: &str = "create NAME --image IMAGE [--store PATH] [OPTION]... | start NAME [--net-direct] \
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
    mounts: Vec<(PathBuf, String)>,
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
    // A runner's machine, RUNNER/NAME: asked of this program there (see remote.rs).
    if crate::runner::split(name).is_some() {
        return crate::remote::machine(command, name, rest);
    }
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
    let home = env::var_os("BOTTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".bottle"));
    home.join("krun/machines")
}

/// Whether `name` is a machine there is: this machine's, or a runner's (RUNNER/NAME, its runner
/// added).
pub fn is_machine(name: &str) -> bool {
    match crate::runner::split(name) {
        Some((runner, rest)) => {
            Machine::new(rest).is_ok()
                && crate::runner::registered().is_ok_and(|r| r.contains_key(runner))
        }
        None => Machine::new(name).is_ok_and(|m| m.exists()),
    }
}

struct Machine {
    name: String,
    dir: PathBuf,
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

    /// Its network policy, if it was made with one.
    fn network(&self) -> PathBuf {
        self.dir.join("network.json")
    }

    /// Started, if it's stopped and needs nothing to start: it has a network policy of its own.
    fn up(&self) -> io::Result<()> {
        if self.exists() && !self.running() && self.network().exists() {
            self.start(&crate::boot::Net::None)?;
        }
        Ok(())
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
        let own;
        let net = if self.network().exists() {
            if *net != crate::boot::Net::None {
                return Err(other(format!("{} has a network of its own", self.name)));
            }
            // Its caching CA, for its policy's hosts as they are: made now if it isn't.
            let hosts = crate::boot::policy(&self.network())?.terminated_hosts();
            bottle_agent_client::ca::certificate(&self.name, &hosts)?;
            own = crate::boot::Net::Policy(self.network());
            &own
        } else {
            net
        };
        let socket = self.socket();
        if let Some(dir) = socket.parent() {
            fs::create_dir_all(dir)?;
            // Only this user's: whoever can reach the socket can run anything in the machine.
            fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        }
        let _ = fs::remove_file(&socket);
        // The last run's logs kept, for when it ended badly and this is starting it again.
        for name in ["boot.log", "console.log"] {
            let _ = fs::rename(self.dir.join(name), self.dir.join(format!("{name}.1")));
        }
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
            .args(match net {
                // A cached host's https answered with the machine's own caching CA.
                crate::boot::Net::Policy(_) => vec!["--ca", &self.name],
                _ => vec![],
            })
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
        // In /, not this command's directory, which may be deleted while the machine runs.
        boot.stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .current_dir("/");
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

fn create(machine: &Machine, args: &[String]) -> io::Result<()> {
    let mut image = None;
    let mut store = None;
    let mut config = Config {
        image: Vec::new(),
        cpus: 2,
        mem_mib: 2048,
        storage_gib: 64,
        env: Vec::new(),
        mounts: Vec::new(),
        run: None,
        user: None,
        workdir: None,
        labels: Default::default(),
    };
    let mut network = None;
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
            "--image" => image = Some(value()?),
            "--store" => store = Some(PathBuf::from(value()?)),
            // As many as the hypervisor has for it, at most: boot sees to that.
            "--cpus" => config.cpus = number(value()?).map(|n: u64| n.min(255) as u8)?,
            "--mem" => config.mem_mib = number(value()?)? as u32,
            "--storage" => config.storage_gib = number(value()?)?,
            "-e" => {
                let pair = value()?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| other(format!("-e {pair}: not NAME=VALUE")))?;
                config.env.push((k.into(), v.into()));
            }
            "--user" => config.user = Some(value()?),
            "--workdir" => config.workdir = Some(value()?),
            // Refused now, not when it's started.
            "--network" => network = Some(crate::boot::policy_given(&value()?)?),
            "--label" => {
                let pair = value()?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| other(format!("--label {pair}: not NAME=VALUE")))?;
                config.labels.insert(k.into(), v.into());
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
    let image = image.ok_or_else(|| other("no --image"))?;
    if machine.exists() {
        return Err(other(format!("there's a machine {} already", machine.name)));
    }
    let (disks, image_env, run) = image_of(&disk_of(&image, store.as_deref())?)?;
    // The image's environment first: a machine's own -e overrides it.
    config.env.splice(0..0, image_env);
    (config.image, config.run) = (disks, run);
    let mke2fs = crate::mke2fs()?;
    fs::create_dir_all(&machine.dir)?;
    let made = (|| {
        bottle_disk::mke2fs::empty(&mke2fs, &machine.disk(), config.storage_gib << 30)?;
        if let Some(network) = &network {
            fs::write(machine.network(), network)?;
        }
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

/// The top stage's disk of `image`: a stage's disk as given, else a tag in `store` (its default, if
/// None).
fn disk_of(image: &str, store: Option<&Path>) -> io::Result<PathBuf> {
    if Path::new(image).is_file() {
        return Ok(image.into());
    }
    let dir = store.map_or_else(crate::build::default_store, Path::to_path_buf);
    let store = Store {
        dir: dir.clone(),
        mke2fs: PathBuf::new(),
    };
    let name = store.resolve(image).map_err(|e| {
        other(format!(
            "{image}: not a disk, nor an image in {}: {e}",
            dir.display()
        ))
    })?;
    store
        .disks(&name)?
        .pop()
        .ok_or_else(|| other(format!("{image} has no stages")))
}

/// An image's disks, environment and what it runs (see image_of).
type Image = (Vec<PathBuf>, Vec<(String, String)>, Option<Vec<String>>);

/// The disks of the image whose top stage's disk is `disk`, bottom first, its environment, and what
/// it runs (its entrypoint, then its command): its store says, if it's in one; else it's an image
/// of one stage, with no environment, that runs nothing.
fn image_of(disk: &Path) -> io::Result<Image> {
    let disk = fs::canonicalize(disk).map_err(|e| other(format!("{}: {e}", disk.display())))?;
    let (Some(dir), Some(name)) = (disk.parent(), disk.file_stem().and_then(|s| s.to_str())) else {
        return Ok((vec![disk], Vec::new(), None));
    };
    if !dir.join(format!("{name}.json")).exists() {
        return Ok((vec![disk], Vec::new(), None));
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
    Ok((
        store.disks(name)?,
        stage.env.into_iter().collect(),
        Some(run).filter(|r| !r.is_empty()),
    ))
}

/// `start NAME [--net-direct]`: the network is its policy's, if it has one; else the start's.
fn start(machine: &Machine, args: &[String]) -> io::Result<()> {
    let mut net = crate::boot::Net::None;
    let mut given = args.iter();
    while let Some(arg) = given.next() {
        match net.take(arg, || given.next().cloned()) {
            Some(taken) => taken.map_err(other)?,
            None => return Err(other(format!("unknown option {arg}"))),
        }
    }
    machine.start(&net)
}

/// The hosts whose TLS machine `name`'s network policy has terminated here, for its caching CA;
/// None if it has no policy of its own (or there's no such machine).
pub fn terminated_hosts(name: &str) -> io::Result<Option<Vec<String>>> {
    let machine = Machine::new(name)?;
    if !machine.network().exists() {
        return Ok(None);
    }
    Ok(Some(
        crate::boot::policy(&machine.network())?.terminated_hosts(),
    ))
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

fn delete(machine: &Machine, args: &[String]) -> io::Result<()> {
    machine.must_exist()?;
    if let Some(arg) = args.iter().find(|a| *a != "-f") {
        return Err(other(format!("unknown option {arg}")));
    }
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
    fs::remove_dir_all(&machine.dir)?;
    // Its caching CA too (see `bottle-cli ca`): another machine of its name gets a new one.
    bottle_agent_client::ca::forget(&machine.name)
}

/// `configure NAME --network FILE|-`: a stopped machine's network policy, given again (its
/// creator's, when what it was made with changes).
fn configure(machine: &Machine, args: &[String]) -> io::Result<()> {
    let network = match args {
        [flag, network] if flag == "--network" => network,
        _ => return Err(other("configure takes --network FILE|-")),
    };
    machine.must_exist()?;
    if machine.running() {
        return Err(other(format!("{} is running: stop it first", machine.name)));
    }
    let network = crate::boot::policy_given(network)?;
    let staged = machine.dir.join(".network.json");
    fs::write(&staged, network)?;
    fs::rename(staged, machine.network())
}

/// `rename NAME NEW`: a stopped machine, under another name; its disk, config and logs with it.
fn rename(machine: &Machine, args: &[String]) -> io::Result<()> {
    let [to] = args else {
        return Err(other("rename takes the machine's new name"));
    };
    let new = Machine::new(to)?;
    machine.must_exist()?;
    if machine.running() {
        return Err(other(format!("{} is running: stop it first", machine.name)));
    }
    if new.exists() || new.dir.exists() {
        return Err(other(format!("there's a machine {} already", new.name)));
    }
    let _ = fs::remove_file(machine.dir.join("pid"));
    fs::rename(&machine.dir, &new.dir)?;
    // Its caching CA is its old name's: one is made for the new name as it's next started.
    bottle_agent_client::ca::forget(&machine.name)
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
                found.push((name, state, machine.config().ok()));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rows: Vec<_> = found
        .iter()
        .map(|(name, state, config)| {
            let mut row = serde_json::json!({"name": name, "state": state});
            if let Some(config) = config {
                row["cpus"] = config.cpus.into();
                row["mem_mib"] = config.mem_mib.into();
                row["storage_gib"] = config.storage_gib.into();
                row["image"] = image_name(&config.image).into();
                row["labels"] = serde_json::to_value(&config.labels).unwrap();
            }
            row
        })
        .collect();
    // And every runner's, RUNNER/NAME (see remote.rs).
    rows.extend(crate::remote::listed());
    if json {
        println!("{}", serde_json::Value::Array(rows));
        return Ok(());
    }
    let text = |row: &serde_json::Value, key: &str| match &row[key] {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut table = vec![[
        "NAME".to_string(),
        "STATE".into(),
        "CPUS".into(),
        "MEMORY".into(),
        "IMAGE".into(),
        "LABELS".into(),
    ]];
    for row in &rows {
        let labels = row["labels"].as_object().map_or(String::new(), |labels| {
            labels
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join(",")
        });
        let memory = row["mem_mib"]
            .as_u64()
            .map_or(String::new(), |m| format!("{m}M"));
        table.push([
            text(row, "name"),
            text(row, "state"),
            text(row, "cpus"),
            memory,
            text(row, "image"),
            labels,
        ]);
    }
    let widths: Vec<usize> = (0..6)
        .map(|i| table.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    for row in table {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    Ok(())
}

/// What a machine's image is called: a tag of its top stage in its store, else the stage's name.
fn image_name(disks: &[PathBuf]) -> String {
    let Some(top) = disks.last() else {
        return String::new();
    };
    let stage = top
        .file_stem()
        .map_or(String::new(), |s| s.to_string_lossy().into_owned());
    let store = Store {
        dir: top.parent().map(Path::to_path_buf).unwrap_or_default(),
        mke2fs: PathBuf::new(),
    };
    store
        .tags()
        .ok()
        .and_then(|tags| {
            tags.into_iter()
                .find(|(_, name)| *name == stage)
                .map(|(tag, _)| tag)
        })
        .unwrap_or(stage)
}

fn status(machine: &Machine, args: &[String]) -> io::Result<()> {
    if let Some(arg) = args.iter().find(|a| *a != "--json") {
        return Err(other(format!("unknown option {arg}")));
    }
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
fn exec(machine: &Machine, rest: Vec<String>) -> ExitCode {
    if !machine.running() {
        eprintln!("bottle-cli: {} isn't running", machine.name);
        return ExitCode::from(FAILED);
    }
    // Its creator's user and directory, unless the command says.
    let options = &rest[..rest.iter().position(|a| a == "--").unwrap_or(rest.len())];
    let given = |flag: &str| options.iter().any(|a| a == flag);
    let mut defaults = Vec::new();
    if let Ok(config) = machine.config() {
        if let Some(user) = config.user.filter(|_| !given("-u")) {
            defaults.extend(["-u".to_string(), user]);
        }
        if let Some(dir) = config.workdir.filter(|_| !given("-w")) {
            defaults.extend(["-w".to_string(), dir]);
        }
    }
    let socket = machine.socket().to_string_lossy().into_owned();
    crate::exec::main(std::iter::once(socket).chain(defaults).chain(rest))
}

/// `info [--json]`: this machine's memory (MiB) and cores, and where bottle's things are on it
/// ($BOTTLE_HOME).
pub fn info(args: impl Iterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.collect();
    if !(args.is_empty() || args == ["--json"]) {
        return crate::usage_error(USAGE);
    }
    // SAFETY: sysconf has no preconditions.
    let (pages, page) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let memory_mib = (pages.max(0) as u64 * page.max(0) as u64) >> 20;
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let home = crate::runner::home();
    if args.is_empty() {
        println!(
            "memory  {memory_mib} MiB\ncpus    {cpus}\nhome    {}",
            home.display()
        );
    } else {
        println!(
            "{}",
            serde_json::json!({"memory_mib": memory_mib, "cpus": cpus, "home": home})
        );
    }
    ExitCode::SUCCESS
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
