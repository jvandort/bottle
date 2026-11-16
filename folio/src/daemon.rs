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
    #[cfg(test)]
    if fake::on() {
        return fake::bind(bind_message(binding)?);
    }
    request(&bind_message(binding)?, true).map(|_| ())
}

/// Stop answering `bottle`'s machine, if folio serve is running at all.
pub fn release(bottle: &str) -> Result<(), String> {
    #[cfg(test)]
    if fake::on() {
        return fake::release(bottle);
    }
    request(&json!({"op": "release", "bottle": bottle}), false).map(|_| ())
}

/// Start folio serve in the background. False if it was already running this program.
pub fn start() -> Result<bool, String> {
    #[cfg(test)]
    if fake::on() {
        fake::CALLS.lock().unwrap().push("start".into());
        return Ok(false);
    }
    match send(&json!({"op": "ping"}), DEFAULT_TIMEOUT) {
        Err(Failure::NotRunning) => {
            start_daemon()?;
            Ok(true)
        }
        // On its way out: this one starts once it's gone. (Python's folio said nothing, and failed.)
        Err(Failure::Closed) => {
            wait_gone();
            start_daemon()?;
            Ok(true)
        }
        Err(Failure::Other(e)) => Err(e),
        Ok(_) => ensure_current(),
    }
}

/// Stop folio serve and every egress it serves, and wait for it to be gone, so a command after
/// finds none (Python's folio returned as it wound up). False if it wasn't running.
pub fn stop() -> Result<bool, String> {
    crate::log::log!("daemon", "asking folio serve to stop");
    let reply = match send(&json!({"op": "shutdown"}), DEFAULT_TIMEOUT) {
        Err(Failure::NotRunning) => return Ok(false),
        // On its way out already.
        Err(Failure::Closed) => {
            wait_gone();
            return Ok(true);
        }
        Err(Failure::Other(e)) => return Err(e),
        Ok(reply) => reply,
    };
    if reply.get("ok") != Some(&Value::Bool(true)) {
        let error = reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("shutdown failed");
        return Err(format!("folio serve: {error}"));
    }
    wait_gone();
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
    crate::log::say!(
        "daemon",
        "folio: folio serve was running other code; restarting it"
    );
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
        Err(Failure::NotRunning) => {
            return Err(format!(
                "folio serve didn't start; see {}",
                log_path().display()
            ));
        }
        Err(Failure::Other(e)) => return Err(e),
    };
    if reply.get("ok") != Some(&Value::Bool(true)) {
        let error = reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("request failed");
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
    let restore = Value::Array(crate::bottles::bind_messages()?).to_string();
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
    crate::log::log!("daemon", "starting folio serve");
    #[cfg(test)]
    if let Some(start) = *fake::STARTS.lock().unwrap() {
        start();
        return Ok(());
    }
    launch(true)?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        match send(&json!({"op": "ping"}), DEFAULT_TIMEOUT) {
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
pub mod fake {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::Value;

    /// What's bound, by bottle: its bind message.
    pub static BOUND: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    /// Each bind, release and start, in order.
    pub static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    /// What starting folio serve does, in a test that says.
    pub static STARTS: Mutex<Option<fn()>> = Mutex::new(None);

    /// Whether a test has folio serve faked: from reset() on, until off().
    static ON: AtomicBool = AtomicBool::new(false);

    pub fn on() -> bool {
        ON.load(Ordering::Relaxed)
    }

    pub fn off() {
        ON.store(false, Ordering::Relaxed);
    }

    pub fn reset() {
        BOUND.lock().unwrap().clear();
        CALLS.lock().unwrap().clear();
        ON.store(true, Ordering::Relaxed);
    }

    pub fn bind(message: Value) -> Result<(), String> {
        let bottle = message["bottle"].as_str().unwrap_or_default().to_string();
        CALLS.lock().unwrap().push(format!("bind {bottle}"));
        let mut bound = BOUND.lock().unwrap();
        bound.retain(|m| m["bottle"] != bottle.as_str());
        bound.push(message);
        Ok(())
    }

    pub fn release(bottle: &str) -> Result<(), String> {
        CALLS.lock().unwrap().push(format!("release {bottle}"));
        BOUND.lock().unwrap().retain(|m| m["bottle"] != bottle);
        Ok(())
    }

    pub fn bound() -> Vec<String> {
        BOUND
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["bottle"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;

    /// folio serve's control socket, as the tests have it: answering with `code` while it runs, and
    /// once it's told to shut down, taking `closing` more connections and closing them unanswered
    /// (what one winding up does) before it's gone.
    struct State {
        code: Option<String>,
        closing: usize,
        calls: Vec<String>,
    }

    static STATE: Mutex<State> = Mutex::new(State {
        code: None,
        closing: 0,
        calls: Vec::new(),
    });

    fn calls() -> Vec<String> {
        STATE.lock().unwrap().calls.clone()
    }

    /// A folio serve running `code` (None: one from before it said), on the control socket.
    fn run(code: Option<String>) {
        let listener = UnixListener::bind(control()).unwrap();
        STATE.lock().unwrap().code = Some(code.unwrap_or_default());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                let mut state = STATE.lock().unwrap();
                if state.code.is_none() {
                    // Winding up: closed unanswered, until it's gone.
                    state.closing = state.closing.saturating_sub(1);
                    state.calls.push("closed".into());
                    if state.closing == 0 {
                        let _ = std::fs::remove_file(control());
                        return;
                    }
                    continue;
                }
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let message: Value = serde_json::from_str(&line).unwrap();
                let op = message["op"].as_str().unwrap().to_string();
                state.calls.push(op.clone());
                let reply = match op.as_str() {
                    "ping" if state.code.as_deref() == Some("") => json!({"ok": true}),
                    "ping" => json!({"ok": true, "code": state.code}),
                    "shutdown" => {
                        state.code = None;
                        if state.closing == 0 {
                            let _ = std::fs::remove_file(control());
                            let _ = writeln!(stream, "{}", json!({"ok": true}));
                            return;
                        }
                        json!({"ok": true})
                    }
                    _ => json!({"ok": true}),
                };
                writeln!(stream, "{reply}").unwrap();
            }
        });
    }

    fn start_this() {
        STATE.lock().unwrap().calls.push("start".into());
        run(Some(code_hash()));
    }

    /// A test of folio serve's client: a fresh home, nothing running, and starting it starts this.
    fn fresh(closing: usize) -> tempdir::Isolated {
        let env = tempdir::isolated();
        *STATE.lock().unwrap() = State {
            code: None,
            closing,
            calls: Vec::new(),
        };
        CURRENT.store(false, Ordering::Relaxed);
        fake::off();
        *fake::STARTS.lock().unwrap() = Some(start_this);
        env
    }

    #[test]
    fn its_sockets_are_in_folios_home_or_short_for_a_deep_one() {
        let env = fresh(0);
        assert_eq!(control(), env.home().join("serve.sock"));
        assert_eq!(
            services_sockets()["ssh"],
            env.home().join("services/ssh.sock")
        );
        let deep = format!("/Users/me/{}", "x".repeat(120));
        tempdir::set(&env, "FOLIO_HOME", &deep);
        let path = control().display().to_string();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        assert!(
            path.len() <= MAX_SOCKET_PATH && path.starts_with(&format!("/tmp/bottle-{uid}/")),
            "{path}"
        );
    }

    #[test]
    fn one_on_other_code_is_restarted_before_its_asked_anything_once_a_command() {
        let _env = fresh(0);
        run(Some("old".into()));
        request(&json!({"op": "bind", "bottle": "b"}), true).unwrap();
        assert_eq!(calls(), ["ping", "shutdown", "start", "bind"]);
        request(&json!({"op": "release", "bottle": "b"}), false).unwrap();
        assert_eq!(calls().last().unwrap(), "release");
        assert_eq!(calls().len(), 5);
    }

    #[test]
    fn one_that_closes_connections_as_it_stops_is_waited_out() {
        let _env = fresh(2);
        run(Some("old".into()));
        request(&json!({"op": "bind", "bottle": "b"}), true).unwrap();
        assert_eq!(
            calls(),
            ["ping", "shutdown", "closed", "closed", "start", "bind"]
        );
    }

    #[test]
    fn one_from_before_it_said_its_code_is_restarted() {
        let _env = fresh(0);
        run(None);
        assert!(start().unwrap()); // started: the one that was running is gone
        assert_eq!(calls(), ["ping", "ping", "shutdown", "start"]);
    }

    #[test]
    fn starting_one_on_its_way_out_waits_it_out_and_starts_this_and_stopping_waits_till_its_gone() {
        let _env = fresh(2);
        run(Some(code_hash()));
        STATE.lock().unwrap().code = None; // told to shut down by someone else: winding up
        assert!(start().unwrap());
        assert_eq!(calls(), ["closed", "closed", "start"]);
        drop(_env);
        let _env = fresh(2);
        run(Some(code_hash()));
        assert!(stop().unwrap());
        assert_eq!(calls(), ["shutdown", "closed", "closed"]);
        assert!(!control().exists());
    }

    #[test]
    fn one_on_this_code_is_left_alone_and_none_is_started_by_what_needs_it() {
        let _env = fresh(0);
        run(Some(code_hash()));
        request(&json!({"op": "bind", "bottle": "b"}), true).unwrap();
        assert_eq!(calls(), ["ping", "bind"]);
        drop(_env);
        let _env = fresh(0);
        request(&json!({"op": "bind", "bottle": "b"}), true).unwrap();
        assert_eq!(calls(), ["start", "bind"]);
        // Releasing from one that isn't running starts nothing; stopping it says it wasn't.
        drop(_env);
        let _env = fresh(0);
        request(&json!({"op": "release", "bottle": "b"}), false).unwrap();
        assert!(!stop().unwrap());
        assert!(calls().is_empty());
    }
}
