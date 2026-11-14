//! `folio-cli serve`: folio serve, which answers what bottles' machines route to it.
//!
//!     folio-cli serve --control PATH --service NAME=PATH... [--restore]
//!
//! bottle-cli decides a bottle's connections itself, by the policy its machine was made with, and
//! gives folio serve the ones that policy routes here, on a unix socket per service (--service:
//! host, ssh, credentials), each starting with a PROXY header saying which machine it's from
//! (proxy.rs). A machine is answered as the bottle it was bound as (gateway.rs): bottle.host's git,
//! messages and list of bottles (host.rs), ssh to the bottles it may reach (ssh.rs), and its
//! credentials' hosts (credentials.rs). A connection from a machine no bottle is is dropped.
//!
//! What folio asks of it comes on --control, a JSON line per request, answered with one:
//! `{"op": "ping"}` (it says the code it runs: a hash of this program), `{"op": "bind", ...}` (a
//! bottle: its machine, runner, repos, features, permissions, labels and ssh grants, all it needs of
//! the bottle, so folio serve reads no record of folio's; and, for one on a runner, what to forward
//! there), `{"op": "release", "bottle": NAME}` (it's gone: with none left, folio serve stops), and
//! `{"op": "shutdown"}`. With --restore, before it answers anything, it binds the bottles a JSON
//! array on stdin says: those whose machines were running when it last stopped.
//!
//! For each runner its bottles are on, it forwards its services' sockets there while it answers
//! one or more of them (runner.rs's forward, in a thread of its own). Once it's stopping it winds
//! up while still taking connections, closing them unanswered (what says it's on its way out), and
//! only then is it gone: what's routed here stops first, then its forwards.

mod credentials;
mod gateway;
mod git;
mod host;
mod http;
pub mod log;
mod messages;
mod proxy;
mod ssh;

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use log::log;

use crate::FAILED;

const USAGE: &str = "serve --control PATH --service NAME=PATH... [--restore]";
/// How long a routed connection has to say what it is.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// The open files folio serve asks for: two for each flow it carries (the bottle's side and the far
/// end's), and the processes it runs.
const OPEN_FILES: u64 = 65536;
pub const LOCAL: &str = "local";

/// A bottle folio serve answers, as bind said it.
#[derive(Debug)]
pub struct Bottle {
    pub name: String,
    pub machine: String,
    pub runner: String,
    pub id: String,
    pub repos: Vec<host::Served>,
    pub permissions: Vec<String>,
    pub labels: serde_json::Map<String, Value>,
    /// Its first repo's name, as the list of bottles has it.
    pub repo: Option<String>,
    pub ssh: Vec<Grant>,
    pub injections: Vec<credentials::Injection>,
    /// What's forwarded to its runner while it's answered: THERE=HERE.
    pub forwards: Vec<String>,
}

/// A bottle another may ssh to: as `name`, at `address`, which is bottle `bottle` with id `id` and
/// no other.
#[derive(Debug, Clone)]
pub struct Grant {
    pub name: String,
    pub bottle: String,
    pub id: String,
    pub address: String,
}

impl Bottle {
    fn from_bind(message: &Value) -> Result<Bottle, String> {
        let text = |key: &str| message.get(key).and_then(Value::as_str).map(String::from);
        let texts = |key: &str| -> Vec<String> {
            message
                .get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let name = text("bottle").ok_or("bind needs the bottle")?;
        let machine = text("machine").ok_or("bind needs the machine")?;
        let repos = message
            .get("repos")
            .and_then(Value::as_array)
            .map(|r| {
                r.iter()
                    .map(host::Served::from_json)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let ssh = message
            .get("ssh")
            .and_then(Value::as_array)
            .map(|grants| {
                grants
                    .iter()
                    .map(|g| Grant {
                        name: g["name"].as_str().unwrap_or_default().into(),
                        bottle: g["bottle"].as_str().unwrap_or_default().into(),
                        id: g["id"].as_str().unwrap_or_default().into(),
                        address: g["address"].as_str().unwrap_or_default().into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Bottle {
            injections: credentials::injections_for(&texts("features"))?,
            name,
            machine,
            runner: text("runner").unwrap_or_else(|| LOCAL.into()),
            id: text("id").unwrap_or_default(),
            repos,
            permissions: texts("permissions"),
            labels: message
                .get("labels")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            repo: text("repo"),
            ssh,
            forwards: texts("forwards"),
        })
    }
}

/// The command line, for this machine, that runs `argv` in `bottle`'s machine as `user`, its stdin
/// attached: bottle-cli's exec, here ($BOTTLE_CLI, else bottle-cli on PATH), or a runner's, through
/// this program's `on`.
pub fn exec_command(bottle: &Bottle, argv: &[&str], user: &str) -> Vec<String> {
    let mut command: Vec<String> = if bottle.runner == LOCAL {
        vec![
            std::env::var("BOTTLE_CLI")
                .ok()
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "bottle-cli".into()),
        ]
    } else {
        let me = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "folio-cli".into());
        vec![me, "on".into(), bottle.runner.clone()]
    };
    command.extend([
        "exec".into(),
        bottle.machine.clone(),
        "-u".into(),
        user.into(),
        "--".into(),
    ]);
    command.extend(argv.iter().map(|a| a.to_string()));
    command
}

/// Copies both ways until the far end is done: the bottle's side (`client`, read, and `back`, its
/// other half, written) and the far end's (`to`, written, and `from`, read). The bottle finishing
/// only half-closes the far end (it may still be answering); the far end finishing ends it all.
/// The bytes each way: up, down.
pub fn relay<W: Write + Send + 'static>(
    mut client: UnixStream,
    mut to: W,
    mut from: impl Read,
    mut back: UnixStream,
) -> (u64, u64) {
    let sending = thread::spawn(move || {
        let sent = pass(&mut client, &mut to);
        drop(to);
        sent
    });
    let down = pass(&mut from, &mut back);
    let _ = back.shutdown(Shutdown::Both);
    (sending.join().unwrap_or(0), down)
}

/// Passes on what `from` reads as it comes, until it ends: the bytes passed. Not io::copy, whose
/// splice between a socket and a pipe waits to fill its buffer, where ssh waits for each line.
fn pass(from: &mut impl Read, to: &mut impl Write) -> u64 {
    let mut buf = vec![0; 64 * 1024];
    let mut passed = 0;
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => return passed,
            Ok(n) => {
                if to.write_all(&buf[..n]).and_then(|()| to.flush()).is_err() {
                    return passed;
                }
                passed += n as u64;
            }
        }
    }
}

/// A runner's forward: a thread, told to stop by `stop`.
struct Forward {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

pub struct Server {
    bottles: Mutex<HashMap<String, Arc<Bottle>>>,
    forwards: Mutex<HashMap<String, Forward>>,
    stopping: AtomicBool,
    /// Its wind-up is done: what's left is to go.
    done: AtomicBool,
    control: PathBuf,
    services: Vec<(String, PathBuf)>,
    code: String,
}

impl Server {
    /// The bottle `machine` was bound as.
    fn bottle_of(&self, machine: &str) -> Option<Arc<Bottle>> {
        self.bottles
            .lock()
            .unwrap()
            .values()
            .find(|b| b.machine == machine)
            .cloned()
    }

    /// Bottle `name`, if it's bound.
    pub fn bound(&self, name: &str) -> Option<Arc<Bottle>> {
        self.bottles.lock().unwrap().get(name).cloned()
    }

    /// Every bottle bound.
    pub fn all(&self) -> Vec<Arc<Bottle>> {
        let mut all: Vec<Arc<Bottle>> = self.bottles.lock().unwrap().values().cloned().collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        all
    }

    fn bind(self: &Arc<Self>, bottle: Bottle) {
        let runner = bottle.runner.clone();
        let forwards = bottle.forwards.clone();
        log!(
            "bottle.daemon",
            "{}: bound, machine {} on {runner}",
            bottle.name,
            bottle.machine
        );
        self.bottles
            .lock()
            .unwrap()
            .insert(bottle.name.clone(), Arc::new(bottle));
        if runner != LOCAL && !forwards.is_empty() {
            self.forward(&runner, forwards);
        }
    }

    fn release(&self, name: &str) {
        let released = self.bottles.lock().unwrap().remove(name);
        let Some(released) = released else { return };
        log!("bottle.daemon", "{name}: released");
        let still = self
            .bottles
            .lock()
            .unwrap()
            .values()
            .any(|b| b.runner == released.runner);
        if released.runner != LOCAL && !still {
            self.end_forward(&released.runner);
        }
        if self.bottles.lock().unwrap().is_empty() {
            log!("bottle.daemon", "no bottle left to answer: stopping");
            self.stop();
        }
    }

    /// Forwards `forwards` to `runner`, unless that's under way already.
    fn forward(self: &Arc<Self>, runner: &str, forwards: Vec<String>) {
        let mut all = self.forwards.lock().unwrap();
        if all.get(runner).is_some_and(|f| !f.thread.is_finished()) {
            return;
        }
        log!("bottle.daemon", "forwarding to runner {runner}");
        let stop = Arc::new(AtomicBool::new(false));
        let (told, name) = (stop.clone(), runner.to_string());
        let thread = thread::spawn(move || {
            if let Err(e) =
                crate::runner::forward_until(&name, &forwards, &|| told.load(Ordering::Relaxed))
            {
                log!("bottle.daemon", "forward to runner {name}: {e}");
            }
        });
        all.insert(runner.to_string(), Forward { stop, thread });
    }

    fn end_forward(&self, runner: &str) {
        let forward = self.forwards.lock().unwrap().remove(runner);
        if let Some(forward) = forward {
            log!("bottle.daemon", "no longer forwarding to runner {runner}");
            forward.stop.store(true, Ordering::Relaxed);
            let _ = forward.thread.join();
        }
    }

    fn stop(&self) {
        if !self.stopping.swap(true, Ordering::SeqCst) {
            // Woken, its control socket's loop starts its wind-up.
            let _ = UnixStream::connect(&self.control);
        }
    }

    /// Answers one request on the control socket; closed unanswered once it's stopping.
    fn control(self: &Arc<Self>, stream: UnixStream) {
        let mut line = String::new();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0
            || self.stopping.load(Ordering::SeqCst)
        {
            return;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => json!({"ok": false, "error": format!("not JSON: {e}")}),
            Ok(message) => match message.get("op").and_then(Value::as_str) {
                Some("ping") => json!({"ok": true, "code": self.code}),
                Some("bind") => match Bottle::from_bind(&message) {
                    Ok(bottle) => {
                        self.bind(bottle);
                        json!({"ok": true})
                    }
                    Err(e) => json!({"ok": false, "error": e}),
                },
                Some("release") => {
                    self.release(
                        message
                            .get("bottle")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                    json!({"ok": true})
                }
                Some("shutdown") => {
                    log!("bottle.daemon", "shutdown requested");
                    // Answered first: the wind-up closes what comes after unanswered.
                    let _ = (&stream).write_all(b"{\"ok\": true}\n");
                    self.stop();
                    return;
                }
                op => json!({"ok": false, "error": format!("unknown op {}", json!(op))}),
            },
        };
        let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
    }

    /// A routed connection, as its header says: given to its bottle's gateway, or dropped.
    fn routed(self: &Arc<Self>, mut stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(HEADER_TIMEOUT));
        let header = match proxy::read(&mut stream) {
            Ok(header) => header,
            Err(e) => {
                log!("bottle.services", "services: refused a connection: {e}");
                return;
            }
        };
        let _ = stream.set_read_timeout(None);
        let target = format!(
            "{}:{}{}",
            header.address,
            header.port,
            header
                .name
                .as_ref()
                .map_or(String::new(), |n| format!(" ({n})"))
        );
        let Some(bottle) = header.machine.as_deref().and_then(|m| self.bottle_of(m)) else {
            log!(
                "bottle.services",
                "services: refused {target} from {}, which no bottle here is",
                header.machine.as_deref().unwrap_or("None")
            );
            return;
        };
        gateway::serve(self, &bottle, &header, stream);
    }
}

/// Listens at `path`, its owner alone let in, anything there before gone first.
fn listen(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let _ = fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Raises this process's limit on open files towards OPEN_FILES, as far as the system lets it.
fn raise_open_files() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit read and write the structure they're given.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    let hard = (limit.rlim_max != libc::RLIM_INFINITY).then_some(limit.rlim_max as u64);
    raised(limit.rlim_cur as u64, hard, |soft| {
        let wanted = libc::rlimit {
            rlim_cur: soft as _,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: as above.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &wanted) == 0 }
    })
}

/// The limit on open files raised from `soft` towards OPEN_FILES, by `set`, which says whether the
/// system took it: no higher than `hard` (None: unlimited), and never lowered.
fn raised(soft: u64, hard: Option<u64>, mut set: impl FnMut(u64) -> bool) -> u64 {
    let mut target = hard.map_or(OPEN_FILES, |h| OPEN_FILES.min(h));
    // macOS refuses more than kern.maxfilesperproc, whatever the hard limit says.
    while target > soft {
        if set(target) {
            return target;
        }
        target /= 2;
    }
    soft
}

/// This program's code, as ping says it: a hash of the program itself.
fn code() -> String {
    let exe = std::env::current_exe()
        .and_then(fs::read)
        .unwrap_or_default();
    Sha256::digest(&exe)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn main(args: impl Iterator<Item = String>) -> ExitCode {
    let mut args = args.peekable();
    let (mut control, mut services, mut restore) = (None, Vec::new(), false);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.peek().cloned()) {
            ("--control", Some(path)) => {
                control = Some(PathBuf::from(path));
                args.next();
            }
            ("--service", Some(service)) => {
                let Some((name, path)) = service.split_once('=') else {
                    return crate::usage_error(USAGE);
                };
                services.push((name.to_string(), PathBuf::from(path)));
                args.next();
            }
            ("--restore", _) => restore = true,
            _ => return crate::usage_error(USAGE),
        }
    }
    let Some(control) = control else {
        return crate::usage_error(USAGE);
    };
    match run(control, services, restore) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log!("bottle.daemon", "folio serve failed: {e}");
            eprintln!("folio-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

fn run(control: PathBuf, services: Vec<(String, PathBuf)>, restore: bool) -> io::Result<()> {
    log!("bottle.daemon", "open files: up to {}", raise_open_files());
    if control.exists() {
        if UnixStream::connect(&control).is_ok() {
            log!(
                "bottle.daemon",
                "another folio serve is already running; exiting"
            );
            return Ok(());
        }
        // Left over from one that died.
        fs::remove_file(&control)?;
    }
    if let Some(dir) = control.parent() {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        if fs::metadata(dir)?.uid() != unsafe { libc::getuid() } {
            return Err(io::Error::other(format!(
                "{} is owned by another user",
                dir.display()
            )));
        }
    }
    let server = Arc::new(Server {
        bottles: Mutex::new(HashMap::new()),
        forwards: Mutex::new(HashMap::new()),
        stopping: AtomicBool::new(false),
        done: AtomicBool::new(false),
        control: control.clone(),
        services: services.clone(),
        code: code(),
    });
    let mut routed = Vec::new();
    for (_, path) in &services {
        let listener = listen(path)?;
        let server = server.clone();
        routed.push(thread::spawn(move || {
            for stream in listener.incoming() {
                if server.stopping.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    let server = server.clone();
                    thread::spawn(move || server.routed(stream));
                }
            }
        }));
    }
    let paths: Vec<String> = services
        .iter()
        .map(|(_, p)| p.display().to_string())
        .collect();
    log!(
        "bottle.daemon",
        "services listening on {}",
        paths.join(", ")
    );
    // Before answering anyone: whoever started it may be about to use a bottle whose machine is
    // running, and what it routes here must find it bound.
    if restore {
        let mut given = String::new();
        io::stdin().read_to_string(&mut given)?;
        let bottles: Vec<Value> = serde_json::from_str(given.trim()).unwrap_or_default();
        for message in bottles {
            match Bottle::from_bind(&message) {
                Ok(bottle) => server.bind(bottle),
                Err(e) => log!(
                    "bottle.daemon",
                    "{}: couldn't restore its decisions: {e}",
                    message["bottle"].as_str().unwrap_or("?")
                ),
            }
        }
    }
    let listener = listen(&control)?;
    log!(
        "bottle.daemon",
        "folio serve {} listening on {}",
        std::process::id(),
        control.display()
    );
    let mut winding = None;
    for stream in listener.incoming() {
        if server.done.load(Ordering::SeqCst) {
            break;
        }
        if server.stopping.load(Ordering::SeqCst) && winding.is_none() {
            let server = server.clone();
            winding = Some(thread::spawn(move || {
                // What's routed here stops first; then its forwards, what they'd carry having
                // nowhere to go.
                for (_, path) in &server.services {
                    let _ = fs::remove_file(path);
                }
                let runners: Vec<String> =
                    server.forwards.lock().unwrap().keys().cloned().collect();
                for runner in runners {
                    server.end_forward(&runner);
                }
                log!(
                    "bottle.daemon",
                    "folio serve {} stopped",
                    std::process::id()
                );
                server.done.store(true, Ordering::SeqCst);
                let _ = UnixStream::connect(&server.control);
            }));
        }
        if let Ok(stream) = stream {
            let server = server.clone();
            thread::spawn(move || server.control(stream));
        }
    }
    // The service sockets' loops go with this process.
    drop(routed);
    let _ = fs::remove_file(&control);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_on_open_files_is_raised_as_far_as_its_let() {
        let mut set = Vec::new();
        assert_eq!(
            raised(256, None, |n| {
                set.push(n);
                true
            }),
            OPEN_FILES
        );
        assert_eq!(set, [OPEN_FILES]);
        assert_eq!(raised(256, Some(4096), |_| true), 4096);
        // macOS: an unlimited hard limit, and kern.maxfilesperproc below what's asked for.
        assert_eq!(raised(256, None, |n| n <= 24576), 16384);
        assert_eq!(raised(100_000, None, |_| panic!("never lowered")), 100_000);
    }
}
