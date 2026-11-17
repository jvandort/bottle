//! folio serve, as folio talks to it: on its control socket in folio's home, a JSON line per
//! request (serve/mod.rs says what it answers). bind() has it answer a bottle's machine (starting it,
//! if it isn't running), release() stops that, and start() and stop() are `folio serve start` and
//! `stop`. bottle-cli decides a bottle's connections itself, by the policy its machine was made with
//! (services::network), and gives folio serve the ones that policy routes to it, on a socket per
//! service (services_sockets).
//!
//! A bind says all folio serve needs of a bottle (its machine, runner, repos, features,
//! permissions, labels and ssh grants, and what to forward to its runner), so folio serve reads no
//! record of folio's; when it starts, it's given the bottles whose machines are running
//! (bottles::bind_messages), to answer before anyone else. A command finds it running this
//! program, or restarts it first: one running another may not know what's asked of it.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bottles::SshGrant;

const START_TIMEOUT: Duration = Duration::from_secs(5);
/// macOS caps Unix socket paths at 104 bytes.
const MAX_SOCKET_PATH: usize = 100;
/// Its log, in folio's home.
const LOG: &str = "serve";
/// What it serves, each on a socket of its own, which a machine's routes name (services::network).
const SERVICES: [&str; 3] = ["host", "ssh", "credentials"];

pub fn socket_path(name: &str) -> PathBuf {
    let path = crate::home().join(name);
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return path;
    }
    // A deep home: a short per-user directory instead, keyed by where it would have been.
    let key: String = Sha256::digest(path.as_os_str().as_encoded_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!("/tmp/bottle-{uid}/{key}.sock"))
}

fn control() -> PathBuf {
    socket_path("serve.sock")
}

/// Where folio serve answers what machines route to it, by service: bottle.host's (git, messages),
/// ssh to bottles, and credentials' hosts.
pub fn services_sockets() -> BTreeMap<String, PathBuf> {
    SERVICES
        .iter()
        .map(|name| {
            (
                name.to_string(),
                socket_path(&format!("services/{name}.sock")),
            )
        })
        .collect()
}

/// folio serve's log.
pub fn log_path() -> PathBuf {
    crate::log::path(LOG)
}

/// This program, by a hash of it: what folio serve says it's running when asked (ping).
pub fn code_hash() -> String {
    crate::serve::code()
}

/// A request on folio serve's control socket, a JSON line: {"op": ..., ...}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// What code it runs: Reply's code.
    Ping,
    /// Answer a bottle's machine.
    Bind(Box<Bind>),
    /// Stop answering a bottle's machine: with none left, folio serve stops.
    Release {
        bottle: String,
    },
    Shutdown,
}

/// What folio serve answers a request: whether it did, why not, and to a ping, the code it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Reply {
    pub fn ok() -> Reply {
        Reply {
            ok: true,
            ..Reply::default()
        }
    }

    pub fn refused(error: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            error: Some(error.into()),
            ..Reply::default()
        }
    }
}

/// What folio serve is told of a bottle: its machine, on its runner, and what that routes to folio
/// serve to answer: its repos, features (whose credentials it attaches), permissions, labels and
/// first repo's name (as the list of bottles has them), the bottles it may ssh to, and, on a runner,
/// what to forward there (THERE=HERE).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bind {
    pub bottle: String,
    pub machine: String,
    pub runner: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub repos: Vec<Served>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub ssh: Vec<SshGrant>,
    #[serde(default)]
    pub forwards: Vec<String>,
}

/// One repo a bottle is served -- a mount's -- with its remotes (specs), under bottle.host/git/<at>.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Served {
    /// The repo its remotes are served from; None: it's gone.
    pub git_dir: Option<PathBuf>,
    pub remotes: Vec<String>,
    pub at: String,
}

/// `bind`, told to folio serve: on a runner, what to forward there with it.
pub fn bind_message(mut bind: Bind) -> Result<Request, String> {
    if bind.runner != crate::runners::LOCAL {
        bind.forwards = crate::runners::get(&bind.runner)?.forwards()?;
    }
    Ok(Request::Bind(Box::new(bind)))
}

/// folio serve, as a command talks to it: whether the command has seen it running this program
/// (asked once a command, see ensure_current). Made by each command, as part of what it knows of
/// bottles (bottles::Known), or for `folio serve start`.
#[derive(Default)]
pub struct Serve {
    current: Cell<bool>,
}

impl Serve {
    /// Have folio serve answer a bottle's machine, starting folio serve if it isn't running. Its
    /// credentials are read as it's bound: a login reaches a bottle when it's next started, or when
    /// folio serve is.
    pub fn bind(&self, bind: Bind) -> Result<(), String> {
        self.request(&bind_message(bind)?, true).map(|_| ())
    }

    /// Stop answering `bottle`'s machine, if folio serve is running at all.
    pub fn release(&self, bottle: &str) -> Result<(), String> {
        let release = Request::Release {
            bottle: bottle.into(),
        };
        self.request(&release, false).map(|_| ())
    }

    /// Start folio serve in the background. False if it was already running this program.
    pub fn start(&self) -> Result<bool, String> {
        match send(&Request::Ping, DEFAULT_TIMEOUT) {
            Err(Failure::NotRunning) => {
                start_daemon()?;
                Ok(true)
            }
            // On its way out: this one starts once it's gone.
            Err(Failure::Closed) => {
                wait_gone();
                start_daemon()?;
                Ok(true)
            }
            Err(Failure::Other(e)) => Err(e),
            Ok(_) => self.ensure_current(),
        }
    }
}

/// Stop folio serve and every egress it serves, and wait for it to be gone, so a command after
/// finds none. False if it wasn't running.
pub fn stop() -> Result<bool, String> {
    crate::log::log!("daemon", "asking folio serve to stop");
    let reply = match send(&Request::Shutdown, DEFAULT_TIMEOUT) {
        Err(Failure::NotRunning) => return Ok(false),
        // On its way out already.
        Err(Failure::Closed) => {
            wait_gone();
            return Ok(true);
        }
        Err(Failure::Other(e)) => return Err(e),
        Ok(reply) => reply,
    };
    if !reply.ok {
        let error = reply.error.as_deref().unwrap_or("shutdown failed");
        return Err(format!("folio serve: {error}"));
    }
    wait_gone();
    Ok(true)
}

/// folio serve running this program, if it's running: one running another is restarted first.
/// Running bottles keep their tokens, and the new folio serve binds them again as it starts, so
/// they lose only the connections they open in between. Returns whether it restarted it.
impl Serve {
    fn ensure_current(&self) -> Result<bool, String> {
        if self.current.get() {
            return Ok(false);
        }
        let reply = match send(&Request::Ping, DEFAULT_TIMEOUT) {
            Err(Failure::NotRunning) => return Ok(false), // not running: whoever starts it starts this one
            Err(Failure::Closed) => {
                wait_gone(); // on its way out: whoever starts it next starts this one
                return Ok(false);
            }
            Err(Failure::Other(e)) => return Err(e),
            Ok(reply) => reply,
        };
        self.current.set(true);
        if reply.code.as_deref() == Some(code_hash().as_str()) {
            return Ok(false);
        }
        crate::log::say!(
            "daemon",
            "folio: folio serve was running other code; restarting it"
        );
        stop()?;
        wait_gone();
        start_daemon()?;
        Ok(true)
    }
}

/// Until the folio serve that's stopping is gone, or would the new one find it and leave. Until
/// then it answers, or, winding up, takes a connection and closes it unanswered.
fn wait_gone() {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Err(Failure::NotRunning) = send(&Request::Ping, DEFAULT_TIMEOUT) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

impl Serve {
    fn request(&self, message: &Request, start: bool) -> Result<Reply, String> {
        self.ensure_current()?;
        let op = match message {
            Request::Ping => "ping",
            Request::Bind(_) => "bind",
            Request::Release { .. } => "release",
            Request::Shutdown => "shutdown",
        };
        let reply = match send(message, DEFAULT_TIMEOUT) {
            Err(Failure::NotRunning) if !start => return Ok(Reply::ok()),
            Err(Failure::NotRunning) => {
                start_daemon()?;
                send(message, DEFAULT_TIMEOUT)
            }
            other => other,
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(Failure::Closed) => {
                return Err(format!(
                    "folio serve closed the connection without answering {op}: is it stopping? See {}",
                    log_path().display()
                ));
            }
            Err(Failure::NotRunning) => {
                return Err(format!(
                    "folio serve didn't start; see {}",
                    log_path().display()
                ));
            }
            Err(Failure::Other(e)) => return Err(e),
        };
        if !reply.ok {
            let error = reply.error.as_deref().unwrap_or("request failed");
            return Err(format!("folio serve: {error}"));
        }
        Ok(reply)
    }
}

enum Failure {
    /// No socket, or nothing listening on it.
    NotRunning,
    /// It took the connection and closed it unanswered: what one winding up does.
    Closed,
    Other(String),
}

fn send(message: &Request, timeout: Duration) -> Result<Reply, Failure> {
    let path = control();
    let stream = UnixStream::connect(&path).map_err(|e| match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => Failure::NotRunning,
        _ => Failure::Other(format!("{}: {e}", path.display())),
    })?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let closed = |e: io::Error| match e.kind() {
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => Failure::Closed,
        _ => Failure::Other(format!("folio serve: {e}")),
    };
    (&stream)
        .write_all(format!("{}\n", serde_json::to_string(message).unwrap()).as_bytes())
        .map_err(closed)?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(closed)?;
    if line.is_empty() {
        return Err(Failure::Closed);
    }
    serde_json::from_str(&line)
        .map_err(|e| Failure::Other(format!("folio serve said {line:?}: {e}")))
}

/// folio serve, this program's `serve`, on folio serve's sockets, given the bottles to answer
/// (bottles::bind_messages): in a session of its own and in /, for one that outlives this command (a
/// machine's directory may be deleted while it runs, and then every git it runs fails), its output
/// in its log; else here.
fn launch(background: bool) -> Result<Child, String> {
    let restore = serde_json::to_string(&crate::bottles::bind_messages()?).unwrap();
    let mut command = Command::new(crate::bottle_cli::me());
    command
        .args(["serve", "--control"])
        .arg(control())
        .arg("--restore");
    for (name, path) in services_sockets() {
        command
            .arg("--service")
            .arg(format!("{name}={}", path.display()));
    }
    command
        .env("FOLIO_HOME", crate::home())
        .env("FOLIO_FEATURES", crate::features::dir())
        .env("BOTTLE_CLI", crate::bottle_cli::binary())
        .stdin(Stdio::piped());
    if background {
        let log = crate::log::open(LOG).map_err(|e| format!("{}: {e}", log_path().display()))?;
        command
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .current_dir(Path::new("/"));
        // SAFETY: setsid, a system call with no allocation, and all the child does before it
        // execs.
        unsafe {
            command.pre_exec(|| {
                let _ = rustix::process::setsid();
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|e| format!("folio serve: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(restore.as_bytes());
    drop(stdin);
    Ok(child)
}

fn start_daemon() -> Result<(), String> {
    crate::log::log!("daemon", "starting folio serve");
    launch(true)?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        match send(&Request::Ping, DEFAULT_TIMEOUT) {
            Err(Failure::NotRunning) => std::thread::sleep(Duration::from_millis(50)),
            _ => return Ok(()),
        }
    }
    Err(format!(
        "folio serve didn't start; see {}",
        log_path().display()
    ))
}

/// folio serve, in this process's place: `folio serve start --foreground`.
pub fn run() -> Result<i32, String> {
    let status = launch(false)?.wait().map_err(|e| e.to_string())?;
    Ok(status.code().unwrap_or(1))
}

/// folio serve, as folio's unit tests have it: what it was told, and nothing started.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir;

    #[test]
    fn its_sockets_are_in_folios_home_or_short_for_a_deep_one() {
        let env = tempdir::isolated();
        assert_eq!(control(), env.home().join("serve.sock"));
        assert_eq!(
            services_sockets()["ssh"],
            env.home().join("services/ssh.sock")
        );
        let deep = format!("/Users/me/{}", "x".repeat(120));
        tempdir::set(&env, "FOLIO_HOME", &deep);
        let path = control().display().to_string();
        let uid = rustix::process::getuid().as_raw();
        assert!(
            path.len() <= MAX_SOCKET_PATH && path.starts_with(&format!("/tmp/bottle-{uid}/")),
            "{path}"
        );
    }
}
