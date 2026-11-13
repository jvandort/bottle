//! `bottle-cli runner`: the other machines this one runs machines on, over ssh.
//!
//!     bottle-cli runner add NAME --ssh DESTINATION
//!     bottle-cli runner ls [--json]
//!     bottle-cli runner rm NAME
//!     bottle-cli runner ssh NAME [COMMAND]...
//!     bottle-cli runner check NAME|DESTINATION [--json]
//!     bottle-cli on NAME COMMAND [ARG]...
//!
//! A runner is a Linux machine with /dev/kvm, reached as the user's own ssh reaches it: a
//! DESTINATION from ~/.ssh/config, or user@host, with their keys and agent. ssh runs in batch mode,
//! never asking for a password or a host key: a runner it can't reach without asking is unreachable.
//! Each runner gets one shared connection (ssh's ControlMaster), kept for a while after its last
//! use. A runner has this program and nothing else of bottle's: in ~/.bottle/dist there, copied
//! from the distribution for its architecture beside this one's (`dist-TARGET`, as `cargo xtask
//! dist --target TARGET` makes it; in $BOTTLE_RUNNER_DISTS instead, if it's set) whenever that's
//! another build. Its machines are named
//! `NAME/MACHINE` here (see machine.rs). `runner ssh` is its shared connection, for a command
//! there: what git's GIT_SSH_COMMAND can be, with a URL `NAME:PATH` and GIT_SSH_VARIANT=simple.
//! `runner check` says what a machine (a runner's, or one to add) is and what it lacks, and adds
//! nothing. `on` runs any of this program's commands there, its output and exit status this
//! process's (paths in its arguments are the runner's: `info` says where its $BOTTLE_HOME is),
//! but for `image build`, whose recipe and what it mounts, and its --network file, are this
//! machine's, sent there with it.
//!
//! Runners are listed in $BOTTLE_HOME/runners.json, `{"version": 1, "runners": {NAME: {"ssh":
//! DESTINATION, ...}}}`; what else an entry says (bottle's "detached") is kept as it is.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Output, Stdio};

use serde::{Deserialize, Serialize};

use crate::FAILED;

pub const USAGE: &str = "runner add NAME --ssh DESTINATION | runner ls [--json] | runner rm NAME \
                         | runner ssh NAME [COMMAND]... | runner check NAME|DESTINATION [--json] \
                         | on NAME COMMAND [ARG]...";

/// ssh's own failures, connecting or authenticating, exit 255.
const SSH_FAILED: i32 = 255;
/// How long a runner's shared connection outlives its last use, in seconds.
const PERSIST: u32 = 600;
/// How long to wait for a runner to answer at all, in seconds: one that's off is said to be, soon.
const CONNECT_TIMEOUT: u32 = 10;
/// macOS caps unix socket paths at 104 bytes; a control socket's is ssh's to extend, by up to 17.
const MAX_CONTROL_PATH: usize = 80;
/// Where a runner has this program, under its home.
pub const DIST: &str = ".bottle/dist";
/// What a distribution was built from, as the dev shim stamps it (bottle/prereqs.py).
const STAMP: &str = ".sources";

/// $BOTTLE_HOME, else ~/.bottle.
pub fn home() -> PathBuf {
    env::var_os("BOTTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".bottle"))
}

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
pub struct Entry {
    pub ssh: String,
    /// What else bottle says of it (that it's detached, say).
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

impl Entry {
    pub fn detached(&self) -> bool {
        self.rest.contains_key("detached")
    }
}

fn registry_path() -> PathBuf {
    home().join("runners.json")
}

fn registry() -> io::Result<Registry> {
    match fs::read(registry_path()) {
        Ok(json) => serde_json::from_slice(&json)
            .map_err(|e| other(format!("{}: {e}", registry_path().display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Registry {
            version: 1,
            ..Default::default()
        }),
        Err(e) => Err(e),
    }
}

fn write_registry(registry: &Registry) -> io::Result<()> {
    let path = registry_path();
    fs::create_dir_all(path.parent().unwrap())?;
    let staged = path.with_extension("json.tmp");
    fs::write(&staged, serde_json::to_vec_pretty(registry).unwrap())?;
    fs::rename(staged, path)
}

/// Every runner added, by name.
pub fn registered() -> io::Result<BTreeMap<String, Entry>> {
    Ok(registry()?.runners)
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// `machine` as a runner's: `RUNNER/NAME`, else None (this machine's).
pub fn split(machine: &str) -> Option<(&str, &str)> {
    machine.split_once('/')
}

/// `s` quoted for a POSIX shell.
pub fn quote(s: &str) -> String {
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
pub struct Runner {
    pub name: String,
    pub destination: String,
}

impl Runner {
    /// Runner `name`, as added; refused if there's none, or it's detached (turned off).
    pub fn named(name: &str) -> io::Result<Runner> {
        let entry = registered()?
            .remove(name)
            .ok_or_else(|| other(format!("no runner named {name}")))?;
        if entry.detached() {
            return Err(other(format!("runner {name} is detached")));
        }
        Ok(Runner {
            name: name.to_string(),
            destination: entry.ssh,
        })
    }

    fn control_path(&self) -> PathBuf {
        let path = home().join("ssh").join(format!("{}.sock", self.name));
        if path.as_os_str().len() <= MAX_CONTROL_PATH {
            return path;
        }
        use sha2::Digest;
        let hash = sha2::Sha256::digest(path.as_os_str().as_encoded_bytes());
        let key: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/bottle-{uid}/{key}.sock"))
    }

    /// ssh to it, on its shared connection, `tty` as ssh's -t (Some(true)), -T (Some(false)), or
    /// its own choice (None); the command to run there, if any, to follow.
    pub fn ssh(&self, tty: Option<bool>) -> Command {
        let control = self.control_path();
        if let Some(dir) = control.parent() {
            let _ = fs::create_dir_all(dir);
            let _ = fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        }
        let mut ssh = Command::new(env::var_os("BOTTLE_SSH").unwrap_or("ssh".into()));
        for option in [
            "BatchMode=yes".to_string(),
            format!("ConnectTimeout={CONNECT_TIMEOUT}"),
            "ControlMaster=auto".to_string(),
            format!("ControlPath={}", control.display()),
            format!("ControlPersist={PERSIST}"),
            // A shared connection to a runner that went away (rebooted, say) is given up in 45s.
            "ServerAliveInterval=15".to_string(),
            "ServerAliveCountMax=3".to_string(),
        ] {
            ssh.args(["-o", &option]);
        }
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

    /// This program there, with `args`.
    pub fn cli(&self, args: &[String], tty: Option<bool>) -> Command {
        let mut ssh = self.ssh(tty);
        let quoted: Vec<String> = args.iter().map(|a| quote(a)).collect();
        ssh.arg(format!("\"$HOME/{DIST}/bottle-cli\" {}", quoted.join(" ")));
        ssh
    }

    /// `script` run in its shell, `input` its stdin: refused as unreachable if ssh couldn't get
    /// there. This program's own failures exit 255 too, told apart by what it says.
    pub fn run(&self, script: &str, input: Option<&[u8]>) -> io::Result<Output> {
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
    pub fn reached(&self, output: &Output) -> io::Result<()> {
        let said = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(SSH_FAILED)
            && !said.lines().any(|l| l.starts_with("bottle-cli: "))
        {
            let why = said.lines().last().unwrap_or("ssh failed").to_string();
            return Err(other(format!(
                "runner {} is unreachable ({why})",
                self.name
            )));
        }
        Ok(())
    }

    /// $HOME there, absolute.
    pub fn home(&self) -> io::Result<String> {
        let output = self.run(r#"printf %s "$HOME""#, None)?;
        let home = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() || home.is_empty() {
            return Err(other(format!(
                "runner {}: {}",
                self.name,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(home)
    }

    /// This program there, the build for its architecture beside this one's: copied over, whole,
    /// if it isn't the same build.
    pub fn ensure_dist(&self) -> io::Result<()> {
        let found = self.run(
            &format!(r#"uname -m; cat "$HOME/{DIST}/{STAMP}" 2>/dev/null"#),
            None,
        )?;
        let found = String::from_utf8_lossy(&found.stdout).into_owned();
        let mut lines = found.lines();
        let arch = lines.next().unwrap_or_default().to_string();
        let theirs = lines.next().unwrap_or_default().to_string();
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
        // $BOTTLE_RUNNER_DISTS/dist-TARGET, else beside this program's own distribution.
        let dists = match env::var_os("BOTTLE_RUNNER_DISTS") {
            Some(dir) => PathBuf::from(dir),
            None => env::current_exe()?
                .parent()
                .and_then(|d| d.parent())
                .ok_or_else(|| other("no directory beside this program"))?
                .to_path_buf(),
        };
        let dist = dists.join(format!("dist-{target}"));
        let ours = fs::read_to_string(dist.join(STAMP)).map_err(|_| {
            other(format!(
                "no bottle-cli for {target} at {} (cargo xtask dist --target {target})",
                dist.display()
            ))
        })?;
        if ours.trim() == theirs.trim() {
            return Ok(());
        }
        eprintln!("bottle-cli: copying bottle-cli to runner {}...", self.name);
        let tar = Command::new("tar")
            .args(["-czf", "-", "-C"])
            .arg(&dist)
            .arg(".")
            .output()?;
        if !tar.status.success() {
            return Err(other(format!("tar of {} failed", dist.display())));
        }
        let there = format!(r#""$HOME/{DIST}""#);
        let copied = self.run(
            &format!(
                "set -e; rm -rf {there}.partial; mkdir -p {there}.partial; \
                 tar -xzf - -C {there}.partial; rm -rf {there}; mv {there}.partial {there}"
            ),
            Some(&tar.stdout),
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
        (Some("ssh"), [name, command @ ..]) => {
            return match Runner::named(name) {
                // Its stdin, stdout and terminal this process's: git's, say.
                Ok(runner) => {
                    let mut ssh = runner.ssh(None);
                    ssh.args(command);
                    let e = ssh.exec();
                    eprintln!("bottle-cli: ssh: {e}");
                    ExitCode::from(FAILED)
                }
                Err(e) => {
                    eprintln!("bottle-cli: {e}");
                    ExitCode::from(FAILED)
                }
            };
        }
        _ => return crate::usage_error(USAGE),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

/// Checks a machine, copies this program there, and adds it as runner `name`.
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
            "runner {name} is already {}; `bottle-cli runner rm {name}` first",
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

/// `on NAME COMMAND [ARG]...`: this program's COMMAND on runner NAME, as it is here (see the
/// module's doc): its exit status.
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
        let said = std::thread::spawn(move || {
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
        if status.code() == Some(SSH_FAILED) && !said.iter().any(|l| l.starts_with("bottle-cli: "))
        {
            let why = said.last().cloned().unwrap_or("ssh failed".into());
            return Err(other(format!(
                "runner {} is unreachable ({why})",
                runner.name
            )));
        }
        Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
    };
    run().unwrap_or_else(|e| {
        eprintln!("bottle-cli: {e}");
        ExitCode::from(FAILED)
    })
}

/// `image build RECIPE STORE ...` there: the recipe, what it mounts and its --network file sent
/// as a tar, built from a directory there it's unpacked in.
fn build_there(runner: &Runner, args: &[String]) -> io::Result<(Command, Option<Vec<u8>>)> {
    let recipe_path = args
        .get(2)
        .ok_or_else(|| other("image build needs RECIPE"))?;
    let mut recipe: serde_json::Value = serde_json::from_slice(&fs::read(recipe_path)?)
        .map_err(|e| other(format!("{recipe_path}: {e}")))?;
    let staged = env::temp_dir().join(format!("bottle-on-{}", std::process::id()));
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
        let tar = Command::new("tar")
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

/// Forgets runner `name`. Nothing there is touched; refused while this machine knows of machines
/// there.
fn rm(name: &str) -> io::Result<()> {
    let mut registry = registry()?;
    if !registry.runners.contains_key(name) {
        return Err(other(format!("no runner named {name}")));
    }
    let known = crate::remote::records(name)?;
    if !known.is_empty() {
        return Err(other(format!(
            "runner {name} has machines: {}; delete them first",
            known.join(", ")
        )));
    }
    registry.runners.remove(name);
    write_registry(&registry)
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

    #[test]
    fn a_runners_machine_is_named_runner_slash_name() {
        assert_eq!(split("desktop/foo"), Some(("desktop", "foo")));
        assert_eq!(split("foo"), None);
    }
}
