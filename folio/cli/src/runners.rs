//! Runners: the machines bottles run on (`folio runner`, runner.rs), as bottles use them.
//!
//! This machine is `local`; another is a Linux machine reached over ssh, whose bottle-cli (`on
//! RUNNER`: the runner's cli) has its machines, by their own names, and its images and CAs. What
//! to build (an image's tag, its inputs and their hash) is worked out here, in builds.rs; a runner
//! keeps the images it's built and builds those it's sent. A machine mounts a repo's objects from
//! the repo itself here, and on a runner from a bare mirror of it there, pushed into over this
//! program's ssh as the machine is made.
//!
//! What a machine routes to folio serve goes to its sockets here (services.rs); on a runner, to
//! sockets there, under its user's home (services_sockets), which folio serve forwards here, over
//! one ssh per runner, while it answers bottles there (forwards; `runner forward`). Those there are
//! named for this machine (controller_key), so two machines folio runs on can share a runner.

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bottle_cli::{self, Cli, Create, Exec, Mount};
use crate::lanes::shell_quote;
use crate::repos::{self, Repo};
use crate::{daemon, services};

pub const LOCAL: &str = "local";
/// Where on a runner, under its user's home, folio keeps bare mirrors of the repos its machines
/// mount objects from.
const MIRRORS: &str = ".folio/git";
/// Where on a runner, under its user's home, the sockets folio serve's are forwarded to are, a
/// directory for each machine folio runs on (controller_key).
const SERVICES: &str = ".folio/services";
/// How long a list waits for a runner to answer at all, in seconds: one that's off (turned off,
/// but not detached) doesn't refuse, it says nothing, so each one asked costs this.
pub const LIST_TIMEOUT: u32 = 3;

/// Runner `name`: its machines, by their names there, and the bottle-cli that has them (cli).
pub struct Runner {
    pub name: String,
    pub cli: Cli,
    info: OnceCell<Value>,
}

impl Runner {
    fn new(name: &str) -> Runner {
        Runner {
            name: name.into(),
            cli: if name == LOCAL {
                Cli::Local
            } else {
                Cli::runner(name)
            },
            info: OnceCell::new(),
        }
    }

    fn host(&self) -> Result<&Value, String> {
        if let Some(info) = self.info.get() {
            return Ok(info);
        }
        let info = bottle_cli::info(&self.cli)?;
        Ok(self.info.get_or_init(|| info))
    }

    /// How much memory it has, in MiB: what a bottle's spec there must fit.
    pub fn memory_mib(&self) -> Result<u64, String> {
        if self.name == LOCAL {
            return crate::specs::host_memory_mib();
        }
        self.host()?
            .get("memory_mib")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("runner {}: bottle-cli info says no memory", self.name))
    }

    pub fn cpus(&self) -> Result<u64, String> {
        if self.name == LOCAL {
            // SAFETY: sysconf has no preconditions. Every core online, as Python's os.cpu_count said.
            let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
            return Ok(if online > 0 { online as u64 } else { 1 });
        }
        self.host()?
            .get("cpus")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("runner {}: bottle-cli info says no cpus", self.name))
    }

    /// Where a machine on it reaches folio serve's services, by service: folio serve's sockets,
    /// here; on a runner, sockets there that folio serve forwards here (forwards).
    pub fn services_sockets(&self) -> Result<BTreeMap<String, String>, String> {
        let here = daemon::services_sockets();
        if self.name == LOCAL {
            return Ok(here
                .into_iter()
                .map(|(k, v)| (k, v.display().to_string()))
                .collect());
        }
        let there = format!("{}/{SERVICES}/{}", home(&self.name)?, controller_key());
        Ok(here
            .into_keys()
            .map(|name| (name.clone(), format!("{there}/{name}.sock")))
            .collect())
    }

    /// What folio serve forwards to a runner while it answers bottles there, as `runner forward`
    /// takes it: each of its services' sockets there, =, its own.
    pub fn forwards(&self) -> Result<Vec<String>, String> {
        let here = daemon::services_sockets();
        Ok(self
            .services_sockets()?
            .into_iter()
            .map(|(name, there)| format!("{there}={}", here[&name].display()))
            .collect())
    }

    /// Where on it a machine mounts `repo`'s objects from: the repo's own here, so nothing is
    /// copied; on a runner, a bare mirror of it there, brought up to date by a push from here.
    /// Fetches aren't served from the mirror: they come here, as everything routed does.
    pub fn objects(&self, repo: &Repo) -> Result<PathBuf, String> {
        if self.name == LOCAL {
            return repos::objects_dir(repo);
        }
        let git_dir = repos::objects_dir(repo)?.parent().unwrap().to_path_buf();
        let ssh = [
            bottle_cli::me(),
            "runner".into(),
            "ssh".into(),
            self.name.clone(),
        ];
        let there = format!(
            "{}/{MIRRORS}/{}.git",
            home(&self.name)?,
            mirror_key(&git_dir)
        );
        let made = Command::new(&ssh[0])
            .args(&ssh[1..])
            .arg(format!(
                "[ -d {q} ] || git init -q --bare {q}",
                q = shell_quote(&there)
            ))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if !made.status.success() {
            return Err(format!(
                "runner {}: {}",
                self.name,
                bottle_cli::error_of(&String::from_utf8_lossy(&made.stderr))
            ));
        }
        // git runs this program's ssh, a "simple" one to it: the runner's name for a host, and
        // git's command.
        let ssh_command = ssh[..3]
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let pushed = Command::new("git")
            .arg("-C")
            .arg(&git_dir)
            .args([
                "push",
                "--quiet",
                "--mirror",
                &format!("{}:{there}", self.name),
            ])
            .env("GIT_SSH_COMMAND", ssh_command)
            .env("GIT_SSH_VARIANT", "simple")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if !pushed.status.success() {
            return Err(format!(
                "runner {}: copying {} there failed: {}",
                self.name,
                repo.name,
                String::from_utf8_lossy(&pushed.stderr).trim()
            ));
        }
        Ok(PathBuf::from(there).join("objects"))
    }

    // Its machines, as bottle_cli has them, by their names there.

    /// A stopped machine, with all its cores; its policy's routes to folio serve's services where
    /// it is (services_sockets).
    pub fn create(&self, mut machine: Create) -> Result<(), String> {
        machine.cpus = self.cpus()?;
        if let Some(network) = machine.network.take() {
            machine.network = Some(services::with_sockets(&network, &self.services_sockets()?));
        }
        bottle_cli::create(&machine, &self.cli)
    }

    pub fn configure(&self, machine: &str, network: &Value) -> Result<(), String> {
        bottle_cli::configure(
            machine,
            &services::with_sockets(network, &self.services_sockets()?),
            &self.cli,
        )
    }

    pub fn start(&self, machine: &str) -> Result<(), String> {
        bottle_cli::start(machine, &self.cli)
    }

    pub fn delete(&self, machine: &str) -> Result<(), String> {
        bottle_cli::delete(machine, &self.cli)
    }

    pub fn state(&self, machine: &str) -> Result<Option<String>, String> {
        bottle_cli::state(machine, &self.cli)
    }

    /// Every machine there, by name: its state. A runner is given LIST_TIMEOUT seconds to answer,
    /// not the usual: a list doesn't wait long for one that's off.
    pub fn states(&self) -> Result<BTreeMap<String, String>, String> {
        let cli = match &self.cli {
            Cli::Local => Cli::Local,
            Cli::Runner { name, .. } => Cli::Runner {
                name: name.clone(),
                connect_timeout: Some(LIST_TIMEOUT),
            },
        };
        bottle_cli::states(&cli)
    }

    pub fn exec(
        &self,
        machine: &str,
        argv: &[String],
        user: Option<&str>,
        input: Option<&str>,
    ) -> Result<String, String> {
        let how = Exec {
            user,
            ..Default::default()
        };
        bottle_cli::exec(machine, argv, &how, input, &self.cli)
    }

    pub fn exec_command(&self, machine: &str, argv: &[String], how: &Exec) -> Vec<String> {
        bottle_cli::exec_command(machine, argv, how, &self.cli)
    }

    pub fn mount(&self, repo: &Repo, target: &str) -> Result<Mount, String> {
        Ok(Mount {
            source: self.objects(repo)?,
            target: target.into(),
        })
    }
}

/// A repo's name among a runner's mirrors: its name, and a hash of where it is here.
pub fn mirror_key(git_dir: &std::path::Path) -> String {
    let base = if git_dir.file_name().is_some_and(|n| n == ".git") {
        git_dir.parent().and_then(|p| p.file_name())
    } else {
        git_dir.file_name()
    };
    let name = base.unwrap_or_default().to_string_lossy();
    let name = name.strip_suffix(".git").unwrap_or(&name).to_string();
    let hash: String = Sha256::digest(git_dir.as_os_str().as_encoded_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{name}-{hash}")
}

/// Every remote runner added (`folio runner add`), by name: whether it's detached.
pub fn registered() -> Result<BTreeMap<String, bool>, String> {
    crate::runner::detached().map_err(|e| e.to_string())
}

/// Runner `name`'s user's home there, as runner.rs keeps it (asking the runner once).
pub fn home(name: &str) -> Result<String, String> {
    crate::runner::home(name).map_err(|e| e.to_string())
}

/// This machine, folio's home on it, among those folio runs on: what its sockets on a runner are
/// named for.
pub fn controller_key() -> String {
    let mut name = [0u8; 256];
    // SAFETY: gethostname writes at most the length it's given.
    let host = unsafe {
        libc::gethostname(name.as_mut_ptr().cast(), name.len());
        std::ffi::CStr::from_ptr(name.as_ptr().cast())
            .to_string_lossy()
            .into_owned()
    };
    let key = format!("{host}\0{}", crate::home().display());
    Sha256::digest(key.as_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The runner named `name`: this machine, or one added with `folio runner add`.
pub fn get(name: &str) -> Result<Runner, String> {
    if name != LOCAL && !registered()?.contains_key(name) {
        return Err(format!(
            "no runner named {}; add one with `folio runner add {name} --ssh DESTINATION`",
            crate::repr(name)
        ));
    }
    Ok(Runner::new(name))
}
