//! A machine: its directory, config and disk, and its VM started (the pid file's lock held) and stopped.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, io, thread};

use bottle_agent_client::policy::Decide as _;
use bottle_agent_client::wait_for_agent;
use bottle_formats::listing::State;

use super::{Config, STOP_TIMEOUT, machines, other};

pub(super) struct Machine {
    pub(super) name: String,
    pub(super) dir: PathBuf,
}

impl Machine {
    pub(super) fn new(name: &str) -> io::Result<Machine> {
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

    pub(super) fn exists(&self) -> bool {
        self.dir.join("config.json").exists()
    }

    pub(super) fn must_exist(&self) -> io::Result<()> {
        if self.exists() {
            Ok(())
        } else {
            Err(other(format!("no machine {}", self.name)))
        }
    }

    pub(super) fn config(&self) -> io::Result<Config> {
        let path = self.dir.join("config.json");
        let json = fs::read(&path).map_err(|e| other(format!("{}: {e}", path.display())))?;
        serde_json::from_slice(&json).map_err(|e| other(format!("{}: {e}", path.display())))
    }

    pub(super) fn disk(&self) -> PathBuf {
        self.dir.join("disk.ext4")
    }

    /// Its network policy, if it was made with one.
    pub(super) fn network(&self) -> PathBuf {
        self.dir.join("network.json")
    }

    /// Started, if it's stopped and needs nothing to start: it has a network policy of its own.
    pub(super) fn up(&self) -> io::Result<()> {
        if self.exists() && !self.running() && self.network().exists() {
            self.start(&crate::boot::Net::None)?;
        }
        Ok(())
    }

    /// Its agent's socket: in its directory, unless that's too long a path for a unix socket (104
    /// bytes, on macOS), then in a directory of this user's in /tmp.
    pub(super) fn socket(&self) -> PathBuf {
        let here = self.dir.join("agent.sock");
        if here.as_os_str().len() < 100 {
            return here;
        }
        let uid = rustix::process::getuid().as_raw();
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

    pub(super) fn pid(&self) -> Option<i32> {
        fs::read_to_string(self.dir.join("pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Whether its VM runs: its process holds the lock on its pid file (start gives it the lock,
    /// which goes with it however it ends), so a pid file it left, naming a pid since reused by
    /// something else, says nothing runs.
    pub(super) fn running(&self) -> bool {
        let Ok(file) = fs::File::open(self.dir.join("pid")) else {
            return false;
        };
        // A lock this takes goes as the file closes.
        let taken = file.try_lock().is_ok();
        !taken
    }

    /// Where it is; None if there's no such machine.
    pub(super) fn state(&self) -> Option<State> {
        if !self.exists() {
            None
        } else if self.running() {
            Some(State::Running)
        } else if self.dir.join("started").exists() {
            Some(State::Stopped)
        } else {
            Some(State::Created)
        }
    }

    pub(super) fn start(&self, net: &crate::boot::Net) -> io::Result<()> {
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
        // Its lock, taken here and handed to its process (see running).
        let mut pid_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("pid"))?;
        match pid_file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(other(format!("{} is running already", self.name)));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
        let held = pid_file.as_raw_fd();
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
            let rw = if config.writable.contains(target) {
                ":rw"
            } else {
                ""
            };
            boot.arg("--mount")
                .arg(format!("{}:{target}{rw}", source.display()));
        }
        if let Some(run) = &config.run {
            boot.arg("--run").arg(serde_json::to_string(run).unwrap());
        }
        // In /, not this command's directory, which may be deleted while the machine runs.
        boot.stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .current_dir("/");
        // SAFETY: setsid and fcntl between fork and exec, system calls with no allocation: a
        // session of its own, out of reach of this terminal's signals, and outliving this process;
        // and the lock's descriptor, open until after the spawn, kept open across exec, so its
        // process holds the lock for as long as it runs.
        unsafe {
            boot.pre_exec(move || {
                let _ = rustix::process::setsid();
                let held = std::os::fd::BorrowedFd::borrow_raw(held);
                rustix::io::fcntl_setfd(held, rustix::io::FdFlags::empty())?;
                Ok(())
            });
        }
        let mut child = boot.spawn()?;
        pid_file.set_len(0)?;
        pid_file.write_all(child.id().to_string().as_bytes())?;
        drop(pid_file);
        let answered = wait_for_agent(&socket, crate::BOOT_TIMEOUT, || {
            matches!(child.try_wait(), Ok(None))
        });
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
    pub(super) fn logs(&self) -> String {
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

    pub(super) fn stop(&self) -> io::Result<()> {
        self.must_exist()?;
        let Some(pid) = self.pid().filter(|_| self.running()) else {
            let _ = fs::remove_file(self.dir.join("pid"));
            return Ok(());
        };
        let asked = bottle_agent_client::stop(&self.socket());
        if !self.ends(STOP_TIMEOUT) {
            self.kill(pid);
        }
        let _ = fs::remove_file(self.dir.join("pid"));
        let _ = fs::remove_file(self.socket());
        asked.map_err(|e| other(format!("stopping {}: {e}; killed", self.name)))
    }

    /// Whether its VM ends within `timeout`.
    pub(super) fn ends(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.running() {
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Its VM's process, `pid`, killed, if it's still its (running says so), and waited for.
    pub(super) fn kill(&self, pid: i32) {
        use rustix::process::{Pid, Signal, kill_process};
        if let Some(pid) = Pid::from_raw(pid)
            && self.running()
        {
            let _ = kill_process(pid, Signal::KILL);
            self.ends(Duration::from_secs(5));
        }
    }
}
