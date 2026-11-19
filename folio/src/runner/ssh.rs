//! A runner as ssh reaches it: its shared connection, its distribution kept as this one's, and commands there.

use std::io::Write;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::{env, fs, io};

use super::commands::mark;
use super::dist::{dist_for, sums};
use super::{
    CONNECT_TIMEOUT, DIST, MANIFEST, MAX_CONTROL_PATH, PERSIST, SSH_FAILED, other, registered,
};

/// A runner, as this machine reaches it.
#[derive(Debug, Clone)]
pub(super) struct Runner {
    pub(super) name: String,
    pub(super) destination: String,
    /// How long to wait for it to answer at all, in seconds.
    pub(super) connect_timeout: u32,
}

impl Runner {
    pub(super) fn new(name: impl Into<String>, destination: impl Into<String>) -> Runner {
        Runner {
            name: name.into(),
            destination: destination.into(),
            connect_timeout: CONNECT_TIMEOUT,
        }
    }

    /// Runner `name`, as added; refused if there's none. One detached is attached again, if it
    /// answers; refused if it doesn't.
    pub(super) fn named(name: &str) -> io::Result<Runner> {
        let entry = registered()?
            .remove(name)
            .ok_or_else(|| other(format!("no runner named {name}")))?;
        let runner = Runner::new(name, entry.ssh.clone());
        if entry.detached() {
            runner.found().map_err(|e| {
                other(format!(
                    "runner {name} is detached, and doesn't answer yet ({e}): turn it on, then try again"
                ))
            })?;
            // What's left there is deleted first, by an attach: there's what it has to say.
            let left: Vec<String> = super::left(name)?.into_keys().collect();
            if !left.is_empty() {
                return Err(other(format!(
                    "runner {name} is back, with {} left on it when their threads were taken over: \
                     `folio runner attach {name}` says what each has that its lane doesn't, and deletes it",
                    left.join(", ")
                )));
            }
            mark(name, None)?;
            eprintln!("folio: runner {name} is back, and attached again");
        }
        Ok(runner)
    }

    pub(super) fn control_path(&self) -> PathBuf {
        let path = crate::home()
            .join("ssh")
            .join(format!("{}.sock", self.name));
        if path.as_os_str().len() <= MAX_CONTROL_PATH {
            return path;
        }
        use sha2::Digest;
        let hash = sha2::Sha256::digest(path.as_os_str().as_encoded_bytes());
        let key: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
        let uid = rustix::process::getuid().as_raw();
        PathBuf::from(format!("/tmp/folio-{uid}/{key}.sock"))
    }

    /// ssh to it: $FOLIO_SSH, else ssh, in batch mode, with `options`.
    pub(super) fn bare_ssh(&self, options: &[String]) -> Command {
        let mut ssh = Command::new(env::var_os("FOLIO_SSH").unwrap_or("ssh".into()));
        for option in [
            "BatchMode=yes".to_string(),
            format!("ConnectTimeout={}", self.connect_timeout),
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
    pub(super) fn ssh(&self, tty: Option<bool>) -> Command {
        let control = self.control_path();
        // A shared connection's socket only in a directory that's this user's alone: else, one
        // of its own (slower, but nobody else's to hold).
        let shared = control.parent().map_or(Ok(()), crate::store::private_dir);
        if let Err(e) = shared {
            crate::log::log!("runner", "{}: no shared connection: {e}", self.name);
            let mut ssh = self.bare_ssh(&[]);
            match tty {
                Some(true) => ssh.arg("-t"),
                Some(false) => ssh.arg("-T"),
                None => &mut ssh,
            };
            ssh.args(["--", &self.destination]);
            return ssh;
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
    pub(super) fn cli(&self, args: &[String], tty: Option<bool>) -> Command {
        let mut ssh = self.ssh(tty);
        let quoted: Vec<String> = args.iter().map(|a| crate::shell::quote(a)).collect();
        ssh.arg(format!("\"$HOME/{DIST}/bottle-cli\" {}", quoted.join(" ")));
        ssh
    }

    /// `script` run in its shell, `input` its stdin: refused as unreachable if ssh couldn't get
    /// there. bottle-cli's own failures exit 255 too, told apart by what it says.
    pub(super) fn run(&self, script: &str, input: Option<&[u8]>) -> io::Result<Output> {
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
    pub(super) fn reached(&self, output: &Output) -> io::Result<()> {
        let said = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = said.lines().collect();
        self.reached_by(output.status.code(), &lines)
    }

    /// Refused as unreachable if what exited `code`, having said `said`, was ssh failing to get
    /// there, not bottle-cli there failing.
    pub(super) fn reached_by(&self, code: Option<i32>, said: &[&str]) -> io::Result<()> {
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
    /// Where it's recorded that the runner's distribution was found current, on the shared
    /// connection there is now: `INODE TARGET MANIFEST-HASH`, the connection's control socket's
    /// inode, the distribution's target, and a hash of its manifest here.
    pub(super) fn checked_path(&self) -> PathBuf {
        let mut path = self.control_path().into_os_string();
        path.push(".checked");
        PathBuf::from(path)
    }

    /// What says the runner's distribution is current: its control socket's inode, and what it was
    /// checked against (`target`'s manifest here, hashed).
    pub(super) fn checked_as(&self, target: &str) -> Option<String> {
        use sha2::Digest;
        use std::os::unix::fs::MetadataExt;
        let inode = fs::metadata(self.control_path()).ok()?.ino();
        let manifest = fs::read(dist_for(target).ok()?.join(MANIFEST)).ok()?;
        let hash: String = sha2::Sha256::digest(&manifest)[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Some(format!("{inode} {target} {hash}"))
    }

    /// Whether the runner's distribution was found current on the connection there is now, and
    /// this distribution hasn't changed since: the check, and the hashing of everything there, is
    /// once a connection, not once a command.
    pub(super) fn found_current(&self) -> bool {
        let Ok(recorded) = fs::read_to_string(self.checked_path()) else {
            return false;
        };
        let target = recorded.split(' ').nth(1).unwrap_or_default();
        self.checked_as(target)
            .is_some_and(|now| now == recorded.trim())
    }

    pub(super) fn ensure_dist(&self) -> io::Result<()> {
        if self.found_current() {
            return Ok(());
        }
        let checked = self.check_dist()?;
        if let Some(checked) = self.checked_as(&checked) {
            let _ = fs::write(self.checked_path(), checked);
        }
        Ok(())
    }

    /// The runner's distribution brought up to date with this one: its target.
    pub(super) fn check_dist(&self) -> io::Result<String> {
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
            return Ok(target.to_string());
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
                "folio: removing {} from runner {}...",
                names(&gone),
                self.name
            );
        } else {
            eprintln!(
                "folio: copying {} to runner {}...",
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
        let gone: Vec<String> = gone.iter().map(|n| crate::shell::quote(n)).collect();
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
        Ok(target.to_string())
    }

    /// What it is: its os, arch, and whether it has kvm and git.
    pub(super) fn found(&self) -> io::Result<BTreeMap<String, String>> {
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
    pub(super) fn missing(&self) -> io::Result<Vec<String>> {
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
