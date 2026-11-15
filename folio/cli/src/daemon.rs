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

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
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
        .map(|name| (name.to_string(), socket_path(&format!("services/{name}.sock"))))
        .collect()
}

/// folio serve's log.
pub fn log_path() -> PathBuf {
    crate::home().join("logs").join(format!("{LOG}.log"))
}

/// The log, opened for appending; only its owner can read it, or list the home it's in. The logs
/// name every host the bottles reached, and the home their repos.
fn open_log() -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    for dir in [crate::home(), log_path().parent().unwrap().to_path_buf()] {
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(log_path())?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// This program, by a hash of it: what folio serve says it's running when asked (ping).
pub fn code_hash() -> String {
    crate::serve::code()
}

/// One repo a bottle is served -- a mount's -- with its remotes (specs), under bottle.host/git/<at>.
pub struct Served {
    /// The repo its remotes are served from; None: it's gone.
    pub git_dir: Option<PathBuf>,
    pub remotes: Vec<String>,
    pub at: String,
}

/// What folio serve is told of a bottle.
pub struct Binding<'a> {
    pub bottle: &'a str,
    pub machine: &'a str,
    pub runner: &'a str,
    pub id: &'a str,
    pub repos: Vec<Served>,
    pub features: Vec<String>,
    pub permissions: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub repo: Option<String>,
    pub ssh: Vec<Value>,
}

/// What has folio serve answer what a bottle's machine, on its runner, routes to it: its repos,
/// features (whose credentials it attaches), permissions, labels and first repo's name (as the list
/// of bottles has them), the bottles it may ssh to ({name, bottle, id, address}), and, on a runner,
/// what to forward there.
pub fn bind_message(binding: &Binding) -> Result<Value, String> {
    let forwards = if binding.runner == crate::runners::LOCAL {
        Vec::new()
    } else {
        crate::runners::get(binding.runner)?.forwards()?
    };
    let repos: Vec<Value> = binding
        .repos
        .iter()
        .map(|r| {
            json!({"git_dir": r.git_dir.as_ref().map(|d| d.display().to_string()), "at": r.at,
                   "remotes": r.remotes})
        })
        .collect();
    Ok(json!({
        "op": "bind", "bottle": binding.bottle, "machine": binding.machine, "runner": binding.runner,
        "id": binding.id, "features": binding.features, "permissions": binding.permissions,
        "labels": binding.labels, "repo": binding.repo, "ssh": binding.ssh, "forwards": forwards,
        "repos": repos,
    }))
}

/// Have folio serve answer a bottle's machine, starting folio serve if it isn't running. Its
/// credentials are read as it's bound: a login reaches a bottle when it's next started, or when
/// folio serve is.
pub fn bind(binding: &Binding) -> Result<(), String> {
    request(&bind_message(binding)?, true).map(|_| ())
}

/// Stop answering `bottle`'s machine, if folio serve is running at all.
pub fn release(bottle: &str) -> Result<(), String> {
    request(&json!({"op": "release", "bottle": bottle}), false).map(|_| ())
}

/// Start folio serve in the background. False if it was already running this program.
pub fn start() -> Result<bool, String> {
    match send(&json!({"op": "ping"}), DEFAULT_TIMEOUT) {
        Err(Failure::NotRunning) => {
            start_daemon()?;
            Ok(true)
        }
        _ => ensure_current(),
    }
}

/// Stop folio serve and every egress it serves. False if it wasn't running.
pub fn stop() -> Result<bool, String> {
    let reply = match send(&json!({"op": "shutdown"}), DEFAULT_TIMEOUT) {
        Err(Failure::NotRunning) => return Ok(false),
        Err(Failure::Closed) => Value::Null,
        Err(Failure::Other(e)) => return Err(e),
        Ok(reply) => reply,
    };
    if reply.get("ok") != Some(&Value::Bool(true)) {
        let error = reply.get("error").and_then(Value::as_str).unwrap_or("shutdown failed");
        return Err(format!("folio serve: {error}"));
    }
    Ok(true)
}

/// Whether this process has seen folio serve running this program: asked once a command.
static CURRENT: AtomicBool = AtomicBool::new(false);

/// folio serve running this program, if it's running: one running another is restarted first.
/// Running bottles keep their tokens, and the new folio serve binds them again as it starts, so
/// they lose only the connections they open in between. Returns whether it restarted it.
fn ensure_current() -> Result<bool, String> {
    if CURRENT.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let reply = match send(&json!({"op": "ping"}), DEFAULT_TIMEOUT) {
        Err(Failure::NotRunning) => return Ok(false), // not running: whoever starts it starts this one
        Err(Failure::Closed) => {
            wait_gone(); // on its way out: whoever starts it next starts this one
            return Ok(false);
        }
        Err(Failure::Other(e)) => return Err(e),
        Ok(reply) => reply,
    };
    CURRENT.store(true, Ordering::Relaxed);
    if reply.get("code").and_then(Value::as_str) == Some(code_hash().as_str()) {
        return Ok(false);
    }
    eprintln!("bottle: folio serve was running other code; restarting it");
    stop()?;
    wait_gone();
    start_daemon()?;
    Ok(true)
}

/// Until the folio serve that's stopping is gone, or would the new one find it and leave. Until
/// then it answers, or, winding up, takes a connection and closes it unanswered.
fn wait_gone() {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Err(Failure::NotRunning) = send(&json!({"op": "ping"}), DEFAULT_TIMEOUT) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

fn request(message: &Value, start: bool) -> Result<Value, String> {
    ensure_current()?;
    let op = message["op"].as_str().unwrap_or_default().to_string();
    let reply = match send(message, DEFAULT_TIMEOUT) {
        Err(Failure::NotRunning) if !start => return Ok(json!({"ok": true})),
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
        Err(Failure::NotRunning) => return Err(format!("folio serve didn't start; see {}", log_path().display())),
        Err(Failure::Other(e)) => return Err(e),
    };
    if reply.get("ok") != Some(&Value::Bool(true)) {
        let error = reply.get("error").and_then(Value::as_str).unwrap_or("request failed");
        return Err(format!("folio serve: {error}"));
    }
    Ok(reply)
}

enum Failure {
    /// No socket, or nothing listening on it.
    NotRunning,
    /// It took the connection and closed it unanswered: what one winding up does.
    Closed,
    Other(String),
}

fn send(message: &Value, timeout: Duration) -> Result<Value, Failure> {
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
        .write_all(format!("{message}\n").as_bytes())
        .map_err(closed)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).map_err(closed)?;
    if line.is_empty() {
        return Err(Failure::Closed);
    }
    serde_json::from_str(&line).map_err(|e| Failure::Other(format!("folio serve said {line:?}: {e}")))
}

/// folio serve, this program's `serve`, on folio serve's sockets, given the bottles to answer
/// (bottles::bind_messages): in a session of its own and in /, for one that outlives this command (a
/// machine's directory may be deleted while it runs, and then every git it runs fails), its output
/// in its log; else here.
fn launch(background: bool) -> Result<Child, String> {
    let restore = Value::Array(crate::bottles::bind_messages()?).to_string();
    let mut command = Command::new(crate::bottle_cli::me());
    command
        .args(["serve", "--control"])
        .arg(control())
        .arg("--restore");
    for (name, path) in services_sockets() {
        command.arg("--service").arg(format!("{name}={}", path.display()));
    }
    command
        .env("FOLIO_HOME", crate::home())
        .env("FOLIO_FEATURES", crate::features::dir())
        .env("BOTTLE_CLI", crate::bottle_cli::binary())
        .stdin(Stdio::piped());
    if background {
        let log = open_log().map_err(|e| format!("{}: {e}", log_path().display()))?;
        command
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .current_dir(Path::new("/"));
        // SAFETY: setsid is async-signal-safe, and all the child does before it execs.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
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
    launch(true)?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        match send(&json!({"op": "ping"}), DEFAULT_TIMEOUT) {
            Err(Failure::NotRunning) => std::thread::sleep(Duration::from_millis(50)),
            _ => return Ok(()),
        }
    }
    Err(format!("folio serve didn't start; see {}", log_path().display()))
}

/// folio serve, in this process's place: `folio serve start --foreground`.
pub fn run() -> Result<i32, String> {
    let status = launch(false)?.wait().map_err(|e| e.to_string())?;
    Ok(status.code().unwrap_or(1))
}
