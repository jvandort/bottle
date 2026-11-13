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
//! output, exit status and (for `exec -t` and `shell`) terminal this process's. Paths in its
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
//! `runner detach` is for turning a runner off: it stops its running machines (asking first, on a
//! terminal; told no, or with no one to ask, it stops nothing and stays attached), and marks it
//! detached; `--offline` marks one that's off already, unasked. A detached runner isn't asked
//! anything. Using one again (`on`, `runner ssh`) attaches it again if it answers, and says it's
//! off if it doesn't; `runner attach` does that alone, and names the machines detaching it stopped.
//! `runner rm` forgets one, touching nothing there.
//!
//! Runners are listed in folio's home, runners.json, `{"version": 1, "runners": {NAME: {"ssh":
//! DESTINATION, "detached": {"at": SECONDS, "stopped": [MACHINE]...}}}}`; what else an entry says
//! is kept as it is. With none there yet, bottle-cli's ($BOTTLE_HOME/runners.json, from when
//! runners were bottle-cli's) is taken as it is.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::FAILED;

const USAGE: &str = "runner add NAME --ssh DESTINATION | runner ls [--json] | runner rm NAME \
                     | runner ssh NAME [COMMAND]... | runner check NAME|DESTINATION [--json] \
                     | runner detach NAME [--offline] | runner attach NAME \
                     | runner forward NAME THERE=HERE... | on NAME COMMAND [ARG]...";

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
    #[serde(default = "version")]
    version: u32,
    #[serde(default)]
    runners: BTreeMap<String, Entry>,
}

fn version() -> u32 {
    1
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

/// bottle-cli's list of runners, from when they were its: $BOTTLE_HOME's, else ~/.bottle's.
fn bottles_registry() -> PathBuf {
    env::var_os("BOTTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".bottle"))
        .join("runners.json")
}

fn registry() -> io::Result<Registry> {
    let path = registry_path();
    let json = match fs::read(&path) {
        Ok(json) => json,
        Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::read(bottles_registry()) {
            Ok(json) => json,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Registry {
                    version: 1,
                    ..Default::default()
                });
            }
            Err(e) => return Err(e),
        },
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

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// sha256sum's lines (a sha256, two spaces, a name), by name.
fn sums<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeMap<String, String> {
    lines
        .filter_map(|line| line.split_once("  "))
        .map(|(sum, name)| (name.to_string(), sum.to_string()))
        .collect()
}

/// The bottle-cli folio runs here, its symlinks followed: $BOTTLE_CLI, else bottle-cli on PATH.
fn bottle_cli() -> io::Result<PathBuf> {
    let named = match env::var_os("BOTTLE_CLI").filter(|c| !c.is_empty()) {
        Some(cli) => PathBuf::from(cli),
        None => env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("bottle-cli"))
            .find(|cli| cli.is_file())
            .ok_or_else(|| other("no bottle-cli: install it, or set BOTTLE_CLI"))?,
    };
    fs::canonicalize(&named).map_err(|e| other(format!("{}: {e}", named.display())))
}

/// Where the distribution for `target` is: $FOLIO_RUNNER_DISTS/dist-TARGET, if that's set; else
/// bottle-cli's own, if it's built for `target`; else dist-TARGET beside it.
fn dist_for(target: &str) -> io::Result<PathBuf> {
    if let Some(dists) = env::var_os("FOLIO_RUNNER_DISTS") {
        return Ok(PathBuf::from(dists).join(format!("dist-{target}")));
    }
    let cli = bottle_cli()?;
    let own = cli
        .parent()
        .ok_or_else(|| other("no directory beside bottle-cli"))?;
    let ours = format!("{}-unknown-linux-musl", env::consts::ARCH);
    if cfg!(target_os = "linux") && target == ours {
        return Ok(own.to_path_buf());
    }
    Ok(own
        .parent()
        .ok_or_else(|| other("no directory beside bottle-cli"))?
        .join(format!("dist-{target}")))
}

/// `s` quoted for a POSIX shell.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A runner, as this machine reaches it.
#[derive(Debug, Clone)]
struct Runner {
    name: String,
    destination: String,
}

impl Runner {
    /// Runner `name`, as added; refused if there's none. One detached is attached again, if it
    /// answers; refused if it doesn't.
    fn named(name: &str) -> io::Result<Runner> {
        let entry = registered()?
            .remove(name)
            .ok_or_else(|| other(format!("no runner named {name}")))?;
        let runner = Runner {
            name: name.to_string(),
            destination: entry.ssh.clone(),
        };
        if entry.detached() {
            runner.found().map_err(|e| {
                other(format!(
                    "runner {name} is detached, and doesn't answer yet ({e}): turn it on, then try again"
                ))
            })?;
            mark(name, None)?;
            eprintln!("folio-cli: runner {name} is back, and attached again");
        }
        Ok(runner)
    }

    fn control_path(&self) -> PathBuf {
        let path = crate::home()
            .join("ssh")
            .join(format!("{}.sock", self.name));
        if path.as_os_str().len() <= MAX_CONTROL_PATH {
            return path;
        }
        use sha2::Digest;
        let hash = sha2::Sha256::digest(path.as_os_str().as_encoded_bytes());
        let key: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/folio-{uid}/{key}.sock"))
    }

    /// ssh to it: $FOLIO_SSH, else ssh, in batch mode, with `options`.
    fn bare_ssh(&self, options: &[String]) -> Command {
        let mut ssh = Command::new(env::var_os("FOLIO_SSH").unwrap_or("ssh".into()));
        for option in [
            "BatchMode=yes".to_string(),
            format!("ConnectTimeout={CONNECT_TIMEOUT}"),
        ]
        .iter()
        .chain(options)
        {
            ssh.args(["-o", option]);
        }
        ssh
    }

    /// ssh to it, on its shared connection, `tty` as ssh's -t (Some(true)), -T (Some(false)), or
    /// its own choice (None); the command to run there, if any, to follow.
    fn ssh(&self, tty: Option<bool>) -> Command {
        let control = self.control_path();
        if let Some(dir) = control.parent() {
            let _ = fs::create_dir_all(dir);
            let _ = fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        }
        let mut ssh = self.bare_ssh(&[
            "ControlMaster=auto".to_string(),
            format!("ControlPath={}", control.display()),
            format!("ControlPersist={PERSIST}"),
            // A shared connection to a runner that went away (rebooted, say) is given up in 45s.
            "ServerAliveInterval=15".to_string(),
            "ServerAliveCountMax=3".to_string(),
        ]);
        match tty {
            Some(true) => {
                ssh.arg("-t");
            }
            Some(false) => {
                ssh.arg("-T");
            }
            None => {}
        }
        ssh.args(["--", &self.destination]);
        ssh
    }

    /// bottle-cli there, with `args`.
    fn cli(&self, args: &[String], tty: Option<bool>) -> Command {
        let mut ssh = self.ssh(tty);
        let quoted: Vec<String> = args.iter().map(|a| quote(a)).collect();
        ssh.arg(format!("\"$HOME/{DIST}/bottle-cli\" {}", quoted.join(" ")));
        ssh
    }

    /// `script` run in its shell, `input` its stdin: refused as unreachable if ssh couldn't get
    /// there. bottle-cli's own failures exit 255 too, told apart by what it says.
    fn run(&self, script: &str, input: Option<&[u8]>) -> io::Result<Output> {
        let mut ssh = self.ssh(Some(false));
        ssh.arg(script)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ssh.spawn()?;
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input)?;
        }
        let output = child.wait_with_output()?;
        self.reached(&output)?;
        Ok(output)
    }

    /// Refused as unreachable if `output`'s ssh couldn't get there.
    fn reached(&self, output: &Output) -> io::Result<()> {
        let said = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = said.lines().collect();
        self.reached_by(output.status.code(), &lines)
    }

    /// Refused as unreachable if what exited `code`, having said `said`, was ssh failing to get
    /// there, not bottle-cli there failing.
    fn reached_by(&self, code: Option<i32>, said: &[&str]) -> io::Result<()> {
        if code != Some(SSH_FAILED) || said.iter().any(|l| l.starts_with("bottle-cli: ")) {
            return Ok(());
        }
        let mut why = said.last().copied().unwrap_or("ssh failed").to_string();
        // What macOS says to a process it doesn't let reach the local network.
        if cfg!(target_os = "macos") && why.contains("No route to host") {
            why.push_str(
                "; on macOS, often the Local Network permission (System Settings > Privacy & \
                 Security > Local Network) of whatever started this",
            );
        }
        Err(other(format!(
            "runner {} is unreachable ({why})",
            self.name
        )))
    }

    /// bottle-cli there, the build for its architecture of the one here: what differs of it copied
    /// over, file by file, by each file's sha256: that distribution's manifest (MANIFEST) against
    /// what sha256sum says of the runner's copy.
    fn ensure_dist(&self) -> io::Result<()> {
        let found = self.run(
            &format!(r#"uname -m; cd "$HOME/{DIST}" 2>/dev/null && sha256sum -- * 2>/dev/null"#),
            None,
        )?;
        let found = String::from_utf8_lossy(&found.stdout).into_owned();
        let mut lines = found.lines();
        let arch = lines.next().unwrap_or_default().to_string();
        let theirs = sums(lines);
        let target = match arch.as_str() {
            "x86_64" => "x86_64-unknown-linux-musl",
            "aarch64" => "aarch64-unknown-linux-musl",
            _ => {
                return Err(other(format!(
                    "runner {} is {}: bottle-cli is built for x86_64 and arm64",
                    self.name,
                    if arch.is_empty() { "unknown" } else { &arch }
                )));
            }
        };
        let dist = dist_for(target)?;
        let ours = fs::read_to_string(dist.join(MANIFEST))
            .map(|listed| sums(listed.lines()))
            .map_err(|_| {
                other(format!(
                    "this bottle-cli has no distribution for {target} (in {}) to copy to runner {}",
                    dist.display(),
                    self.name
                ))
            })?;
        let changed: Vec<&String> = ours
            .iter()
            .filter(|(name, sum)| theirs.get(*name) != Some(sum))
            .map(|(name, _)| name)
            .collect();
        let gone: Vec<&String> = theirs.keys().filter(|n| !ours.contains_key(*n)).collect();
        if changed.is_empty() && gone.is_empty() {
            return Ok(());
        }
        let names = |names: &[&String]| {
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut sent = Vec::new();
        if changed.is_empty() {
            eprintln!(
                "folio-cli: removing {} from runner {}...",
                names(&gone),
                self.name
            );
        } else {
            eprintln!(
                "folio-cli: copying {} to runner {}...",
                names(&changed),
                self.name
            );
            let tar = crate::packing_tar()
                .args(["-czf", "-", "-C"])
                .arg(&dist)
                .arg("--")
                .args(&changed)
                .output()?;
            if !tar.status.success() {
                return Err(other(format!("tar of {} failed", dist.display())));
            }
            sent = tar.stdout;
        }
        // What's changed, over a copy of what's there, with what's gone gone; then that, whole, in
        // its place: it's never half of one build and half of another.
        let there = format!(r#""$HOME/{DIST}""#);
        let gone: Vec<String> = gone.iter().map(|n| quote(n)).collect();
        let copied = self.run(
            &format!(
                "set -e; rm -rf {there}.partial; mkdir -p {there}.partial; \
                 if [ -d {there} ]; then cp -pR {there}/. {there}.partial/; fi; \
                 {unpack} cd {there}.partial; rm -f .sources {gone}; cd ..; \
                 rm -rf {there}; mv {there}.partial {there}",
                unpack = if sent.is_empty() {
                    String::new()
                } else {
                    format!("tar -xzf - -C {there}.partial;")
                },
                gone = gone.join(" ")
            ),
            Some(&sent),
        )?;
        if !copied.status.success() {
            return Err(other(format!(
                "copying bottle-cli to runner {} failed: {}",
                self.name,
                String::from_utf8_lossy(&copied.stderr).trim()
            )));
        }
        Ok(())
    }

    /// What it is: its os, arch, and whether it has kvm and git.
    fn found(&self) -> io::Result<BTreeMap<String, String>> {
        let found = self.run(
            "echo \"os=$(uname -s)\"; echo \"arch=$(uname -m)\"; \
             if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo kvm=yes; else echo kvm=no; fi; \
             if command -v git >/dev/null 2>&1; then echo git=yes; else echo git=no; fi",
            None,
        )?;
        Ok(String::from_utf8_lossy(&found.stdout)
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect())
    }

    /// What it lacks of what it needs to run machines, each as the user would fix it.
    fn missing(&self) -> io::Result<Vec<String>> {
        let found = self.found()?;
        let get = |k: &str| found.get(k).map(String::as_str).unwrap_or_default();
        let mut missing = Vec::new();
        if get("os") != "Linux" {
            missing.push(format!("Linux (it runs {})", get("os")));
        } else if !matches!(get("arch"), "x86_64" | "aarch64") {
            missing.push(format!("x86_64 or arm64 (it's {})", get("arch")));
        }
        if get("kvm") != "yes" {
            missing.push("access to /dev/kvm (add the user to the kvm group)".to_string());
        }
        if get("git") != "yes" {
            missing.push("git".to_string());
        }
        Ok(missing)
    }
}

pub fn main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let command = args.next();
    let args: Vec<String> = args.collect();
    let result = match (command.as_deref(), args.as_slice()) {
        (Some("add"), [name, flag, destination]) if flag == "--ssh" => add(name, destination),
        (Some("ls"), []) => ls(false),
        (Some("ls"), [flag]) if flag == "--json" => ls(true),
        (Some("rm"), [name]) => rm(name),
        (Some("check"), [target]) => check(target, false),
        (Some("check"), [target, flag]) if flag == "--json" => check(target, true),
        (Some("detach"), [name]) => detach(name, false),
        (Some("detach"), [name, flag]) if flag == "--offline" => detach(name, true),
        (Some("attach"), [name]) => attach(name),
        (Some("forward"), [name, forwards @ ..]) if !forwards.is_empty() => forward(name, forwards),
        (Some("ssh"), [name, command @ ..]) => {
            return match Runner::named(name) {
                // Its stdin, stdout and terminal this process's: git's, say.
                Ok(runner) => {
                    let mut ssh = runner.ssh(None);
                    ssh.args(command);
                    let e = ssh.exec();
                    eprintln!("folio-cli: ssh: {e}");
                    ExitCode::from(FAILED)
                }
                Err(e) => {
                    eprintln!("folio-cli: {e}");
                    ExitCode::from(FAILED)
                }
            };
        }
        _ => return crate::usage_error(USAGE),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("folio-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

/// Checks a machine, copies bottle-cli there, and adds it as runner `name`.
fn add(name: &str, destination: &str) -> io::Result<()> {
    if name.is_empty()
        || name == "local"
        || name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(other(format!(
            "invalid runner name {name:?}: use letters, digits, '.', '_' or '-', and not local"
        )));
    }
    let mut registry = registry()?;
    if let Some(known) = registry.runners.get(name)
        && known.ssh != destination
    {
        return Err(other(format!(
            "runner {name} is already {}; `folio runner rm {name}` first",
            known.ssh
        )));
    }
    let runner = Runner {
        name: name.to_string(),
        destination: destination.to_string(),
    };
    let missing = runner.missing()?;
    if !missing.is_empty() {
        return Err(other(format!(
            "{destination} can't be a runner yet; it needs: {}",
            missing.join("; ")
        )));
    }
    runner.ensure_dist()?;
    registry
        .runners
        .entry(name.to_string())
        .or_insert_with(|| Entry {
            ssh: destination.to_string(),
            rest: Default::default(),
        });
    write_registry(&registry)
}

fn ls(json: bool) -> io::Result<()> {
    let runners = registered()?;
    if json {
        let listed: Vec<serde_json::Value> = runners
            .iter()
            .map(|(name, e)| {
                serde_json::json!({"name": name, "ssh": e.ssh, "detached": e.detached()})
            })
            .collect();
        println!("{}", serde_json::Value::Array(listed));
    } else {
        for (name, e) in &runners {
            let detached = if e.detached() { "  (detached)" } else { "" };
            println!("{name}  {}{detached}", e.ssh);
        }
    }
    Ok(())
}

/// Marks runner `name` detached, having stopped `detached` (Some), or attached (None).
fn mark(name: &str, detached: Option<&[String]>) -> io::Result<()> {
    let mut registry = registry()?;
    let entry = registry
        .runners
        .get_mut(name)
        .ok_or_else(|| other(format!("no runner named {name}")))?;
    match detached {
        Some(stopped) => {
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            entry.rest.insert(
                "detached".into(),
                serde_json::json!({"at": at, "stopped": stopped}),
            );
        }
        None => {
            entry.rest.remove("detached");
        }
    }
    write_registry(&registry)
}

/// The machines running on `runner`, by their names there.
fn running(runner: &Runner) -> io::Result<Vec<String>> {
    let output = runner
        .cli(&["ls".into(), "--json".into()], Some(false))
        .stdin(Stdio::null())
        .output()?;
    runner.reached(&output)?;
    if !output.status.success() {
        return Err(other(String::from_utf8_lossy(&output.stderr).trim()));
    }
    let listed: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;
    Ok(listed
        .iter()
        .filter(|m| m["state"] == "running")
        .filter_map(|m| m["name"].as_str().map(String::from))
        .collect())
}

/// `runner detach NAME [--offline]` (see the module's doc).
fn detach(name: &str, offline: bool) -> io::Result<()> {
    let entry = registered()?
        .remove(name)
        .ok_or_else(|| other(format!("no runner named {name}")))?;
    if entry.detached() {
        println!("Runner {name} is detached already: it's safe to turn off");
        return Ok(());
    }
    let mut stopped = Vec::new();
    if !offline {
        let runner = Runner {
            name: name.to_string(),
            destination: entry.ssh,
        };
        let off = |e: io::Error| {
            other(format!(
                "{e}; if it's off already, `folio runner detach {name} --offline`"
            ))
        };
        runner.ensure_dist().map_err(off)?;
        let up = running(&runner).map_err(off)?;
        if !up.is_empty() {
            if !ask(&format!(
                "Running on {name}: {}. Stopping one ends whatever runs in it, its agent's work \
                 included: tell them to wrap up first, if they're busy.\nStop them now? [y/N] ",
                up.join(", ")
            )) {
                return Err(other(format!(
                    "runner {name} is still attached, and {} still running: `folio runner detach \
                     {name}` again when they can be stopped",
                    up.join(", ")
                )));
            }
            for machine in &up {
                let out = runner
                    .cli(&["stop".into(), machine.clone()], Some(false))
                    .stdin(Stdio::null())
                    .output()?;
                runner.reached(&out)?;
                if !out.status.success() {
                    return Err(other(format!("couldn't stop {machine} on {name}")));
                }
                println!("Stopped {machine}");
                stopped.push(machine.clone());
            }
            let still = running(&runner).map_err(off)?;
            if !still.is_empty() {
                return Err(other(format!(
                    "runner {name} still runs {}: `folio runner detach {name}` again",
                    still.join(", ")
                )));
            }
        }
    }
    mark(name, Some(&stopped))?;
    if offline {
        println!("Runner {name} detached, unasked: nothing was stopped or checked");
    } else {
        println!("Runner {name} detached: nothing runs on it, so it's safe to turn off");
    }
    Ok(())
}

/// Whether the user, asked `question` on a terminal, says yes; no, without one.
fn ask(question: &str) -> bool {
    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        return false;
    }
    print!("{question}");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// `runner attach NAME`: attached again, once it answers.
fn attach(name: &str) -> io::Result<()> {
    let entry = registered()?
        .remove(name)
        .ok_or_else(|| other(format!("no runner named {name}")))?;
    let stopped: Vec<String> = entry
        .rest
        .get("detached")
        .and_then(|d| d["stopped"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.as_str().map(String::from))
        .collect();
    Runner {
        name: name.to_string(),
        destination: entry.ssh,
    }
    .found()?;
    mark(name, None)?;
    println!("Runner {name} attached");
    if !stopped.is_empty() {
        println!(
            "Running when it was detached: {}. Their threads' bottles start again when they're next used.",
            stopped.join(", ")
        );
    }
    Ok(())
}

/// What runner `target` (a name), or the machine at DESTINATION `target`, is and lacks.
fn check(target: &str, json: bool) -> io::Result<()> {
    let runner = Runner::named(target).unwrap_or_else(|_| Runner {
        name: target.to_string(),
        destination: target.to_string(),
    });
    let found = runner.found()?;
    let missing = runner.missing()?;
    let get = |k: &str| found.get(k).cloned().unwrap_or_default();
    if json {
        println!(
            "{}",
            serde_json::json!({"os": get("os"), "arch": get("arch"), "kvm": get("kvm") == "yes",
                               "git": get("git") == "yes", "missing": missing})
        );
    } else {
        println!("{} {}", get("os"), get("arch"));
        for m in missing {
            println!("missing: {m}");
        }
    }
    Ok(())
}

/// Forgets runner `name`. Nothing there is touched: what's there of folio's bottles is folio's to
/// have deleted first.
fn rm(name: &str) -> io::Result<()> {
    let mut registry = registry()?;
    if registry.runners.remove(name).is_none() {
        return Err(other(format!("no runner named {name}")));
    }
    write_registry(&registry)
}

/// `runner forward NAME THERE=HERE...` (see the module's doc).
fn forward(name: &str, forwards: &[String]) -> io::Result<()> {
    let runner = Runner::named(name)?;
    let pairs: Vec<(&str, &str)> = forwards
        .iter()
        .map(|f| {
            f.split_once('=')
                .ok_or_else(|| other(format!("{f}: not THERE=HERE")))
        })
        .collect::<io::Result<_>>()?;
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    let gone = || unsafe { libc::getppid() } != parent;
    let mut backoff = Duration::from_secs(1);
    while !gone() {
        let since = Instant::now();
        match connect(&runner, &pairs, &gone) {
            Ok(None) => return Ok(()),
            Ok(Some(why)) => eprintln!("folio-cli: forward to {name} dropped: {why}"),
            Err(e) => eprintln!("folio-cli: forward to {name}: {e}"),
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
fn connect(
    runner: &Runner,
    pairs: &[(&str, &str)],
    gone: &dyn Fn() -> bool,
) -> io::Result<Option<String>> {
    // Whatever a forward that died left there, gone first: ssh won't bind over it.
    let theres: Vec<String> = pairs.iter().map(|(t, _)| quote(t)).collect();
    let dirs: Vec<String> = pairs
        .iter()
        .filter_map(|(t, _)| t.rsplit_once('/').map(|(d, _)| quote(d)))
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
    eprintln!("folio-cli: forwarding to {}", runner.name);
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

/// `on NAME COMMAND [ARG]...`: bottle-cli's COMMAND on runner NAME (see the module's doc): its
/// exit status.
pub fn on(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(name) = args.next() else {
        return crate::usage_error(USAGE);
    };
    let args: Vec<String> = args.collect();
    if args.is_empty() {
        return crate::usage_error(USAGE);
    }
    let run = || -> io::Result<ExitCode> {
        let runner = Runner::named(&name)?;
        runner.ensure_dist()?;
        // Attached wholly, a terminal and all: this process becomes the ssh.
        let attached = args[0] == "shell" || (args[0] == "exec" && args.iter().any(|a| a == "-t"));
        if attached {
            return Err(runner.cli(&args, Some(true)).exec());
        }
        let (mut command, input) = if args.starts_with(&["image".to_string(), "build".to_string()])
        {
            build_there(&runner, &args)?
        } else {
            (runner.cli(&args, Some(false)), None)
        };
        let mut child = command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(&input)?;
        }
        // What it says, said here as it says it; its last lines kept, to tell ssh's failure
        // from its own.
        let stderr = child.stderr.take().unwrap();
        let said = thread::spawn(move || {
            let mut kept: Vec<String> = Vec::new();
            for line in io::BufRead::lines(io::BufReader::new(stderr)).map_while(Result::ok) {
                eprintln!("{line}");
                kept.push(line);
                if kept.len() > 20 {
                    kept.remove(0);
                }
            }
            kept
        });
        let status = child.wait()?;
        let said = said.join().unwrap_or_default();
        let lines: Vec<&str> = said.iter().map(String::as_str).collect();
        runner.reached_by(status.code(), &lines)?;
        Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
    };
    run().unwrap_or_else(|e| {
        eprintln!("folio-cli: {e}");
        ExitCode::from(FAILED)
    })
}

/// `image build RECIPE ...` there: the recipe, what it mounts and its --network file sent as a
/// tar, built from a directory there it's unpacked in.
fn build_there(runner: &Runner, args: &[String]) -> io::Result<(Command, Option<Vec<u8>>)> {
    let recipe_path = args
        .get(2)
        .ok_or_else(|| other("image build needs RECIPE"))?;
    let mut recipe: serde_json::Value = serde_json::from_slice(&fs::read(recipe_path)?)
        .map_err(|e| other(format!("{recipe_path}: {e}")))?;
    let staged = env::temp_dir().join(format!("folio-on-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staged);
    fs::create_dir_all(&staged)?;
    let made = (|| -> io::Result<Vec<u8>> {
        if let Some(source) = recipe
            .pointer("/mount/source")
            .and_then(|s| s.as_str())
            .map(PathBuf::from)
        {
            let source = if source.is_relative() {
                PathBuf::from(recipe_path)
                    .parent()
                    .unwrap_or(&PathBuf::from("."))
                    .join(source)
            } else {
                source
            };
            // A copy: what's sent is the directory's content, under a name of its own.
            let copied = Command::new("cp")
                .arg("-R")
                .arg(&source)
                .arg(staged.join("context"))
                .status()?;
            if !copied.success() {
                return Err(other(format!("copying {} failed", source.display())));
            }
            recipe["mount"]["source"] = "context".into();
        }
        fs::write(
            staged.join("recipe.json"),
            serde_json::to_vec(&recipe).unwrap(),
        )?;
        if let Some(at) = args.iter().position(|a| a == "--network")
            && let Some(network) = args.get(at + 1)
        {
            fs::copy(network, staged.join("network.json"))?;
        }
        let tar = crate::packing_tar()
            .arg("-cf")
            .arg("-")
            .arg("-C")
            .arg(&staged)
            .arg(".")
            .output()?;
        if !tar.status.success() {
            return Err(other("tar of the build's recipe failed"));
        }
        Ok(tar.stdout)
    })();
    let _ = fs::remove_dir_all(&staged);
    let tar = made?;
    let mut there: Vec<String> = args.to_vec();
    there[2] = "\"$d/recipe.json\"".into();
    if let Some(at) = there.iter().position(|a| a == "--network") {
        there[at + 1] = "\"$d/network.json\"".into();
    }
    let quoted: Vec<String> = there
        .iter()
        .map(|a| {
            if a.starts_with("\"$d/") {
                a.clone()
            } else {
                quote(a)
            }
        })
        .collect();
    let script = format!(
        "set -e; d=$(mktemp -d); trap 'rm -rf \"$d\"' EXIT; tar -xf - -C \"$d\"; \
         \"$HOME/{DIST}/bottle-cli\" {}",
        quoted.join(" ")
    );
    let mut ssh = runner.ssh(Some(false));
    ssh.arg(script);
    Ok((ssh, Some(tar)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_for_a_shell() {
        assert_eq!(quote("machine"), "machine");
        assert_eq!(quote("/a/b-c.json"), "/a/b-c.json");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$HOME"), "'$HOME'");
    }
}
