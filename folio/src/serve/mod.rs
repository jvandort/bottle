//! `folio-cli serve`: folio serve, which answers what bottles' machines route to it.
//!
//!     folio-cli serve --control PATH --service NAME=PATH... [--restore]
//!
//! bottle-cli decides a bottle's connections itself, by the policy its machine was made with, and
//! gives folio serve the ones that policy routes here, on a unix socket per service (--service
//! NAME=PATH), each starting with a PROXY header saying which machine it's from and what it was for
//! (proxy.rs). Each socket is one service's, which answers it as the bottle the machine was bound
//! as (service.rs): git (git/: its repos' remotes), messages (messages/: to other bottles), bottles
//! (bottles/: the list of them), ssh (ssh/: to the bottles it may reach) and credentials
//! (credentials/: its credentials' hosts). A connection from a machine no bottle is is dropped.
//!
//! What folio asks of it comes on --control, a JSON line per request, answered with one:
//! `{"op": "ping"}` (it says the code it runs: a hash of this program), `{"op": "bind", ...}` (a
//! bottle: its machine, runner, repos, features, permissions, labels and ssh grants, all it needs of
//! the bottle, so folio serve reads no record of folio's; and, for one on a runner, what to forward
//! there), `{"op": "release", "bottle": NAME}` (it's gone: with none left, folio serve stops),
//! `{"op": "shutdown"}`, and `{"op": "listen", "bottle": NAME}` (what the bottle says to the snapshot
//! address, forwarded down the connection, `{"answer": TEXT}` a line, until it's closed). With --restore, before it answers anything, it binds the bottles a JSON
//! array on stdin says: those whose machines were running when it last stopped.
//!
//! It also snapshots a thread whose agent has been at rest a while (idle.rs), as folio pause would,
//! the bottle kept.
//!
//! For each runner its bottles are on, it forwards its services' sockets there while it answers
//! one or more of them (runner.rs's forward, in a thread of its own), each to a socket here of that
//! runner's own (services/RUNNER/), so a connection is answered only as a bottle bound on the runner
//! it came from: a machine of the same name elsewhere (a thread's old bottle, left on a runner when
//! it was taken over) is none. Once it's stopping it winds
//! up while still taking connections, closing them unanswered (what says it's on its way out), and
//! only then is it gone: what's routed here stops first, then its forwards.

mod bottles;
pub(crate) mod credentials;
mod git;
mod host;
mod http;
mod idle;
pub(crate) mod messages;
mod proxy;
mod service;
mod ssh;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::control::{Bind, Grant, Reply, Request};
use crate::log::log;
use service::{Bound, Caller, Service};

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
    pub repos: Vec<crate::control::Served>,
    pub permissions: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// Its first repo's name, as the list of bottles has it.
    pub repo: Option<String>,
    pub ssh: Vec<Grant>,
    pub injections: Vec<credentials::Injection>,
    /// What's forwarded to its runner while it's answered: THERE=HERE.
    pub forwards: Vec<String>,
}

impl Bottle {
    fn from_bind(bind: Bind) -> Result<Bottle, String> {
        Ok(Bottle {
            injections: credentials::injections_for(&bind.features, &crate::auth::keychain)?,
            repos: bind.repos,
            name: bind.bottle,
            machine: bind.machine,
            runner: bind.runner,
            id: bind.id,
            permissions: bind.permissions,
            labels: bind.labels,
            repo: bind.repo,
            ssh: bind.ssh,
            forwards: bind.forwards,
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
            .unwrap_or_else(|_| "folio".into());
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

/// A service, by its name.
type Route = (String, Arc<dyn Service>);

/// A runner's forward: a thread, told to stop by `stop`, and the sockets it forwards there, which
/// are that runner's alone.
struct Forward {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
    sockets: Vec<PathBuf>,
}

pub struct Server {
    bottles: Mutex<HashMap<String, Arc<Bottle>>>,
    /// Who's listening for what each bottle says to the snapshot address: a connection each, by an
    /// id of its own.
    listening: Mutex<HashMap<String, Vec<(u64, UnixStream)>>>,
    forwards: Mutex<HashMap<String, Forward>>,
    /// Each service, by its socket here.
    routes: Mutex<HashMap<PathBuf, Route>>,
    stopping: AtomicBool,
    /// Its wind-up is done: what's left is to go.
    done: AtomicBool,
    control: PathBuf,
    services: Vec<(String, PathBuf)>,
    code: String,
}

impl Server {
    /// The bottle `machine`, on `runner`, was bound as: a machine of the same name elsewhere (one
    /// left on a runner that was off when its thread was taken over) is none.
    fn bottle_of(&self, machine: &str, runner: &str) -> Option<Arc<Bottle>> {
        self.bottles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|b| b.machine == machine && b.runner == runner)
            .cloned()
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
            .unwrap_or_else(|e| e.into_inner())
            .insert(bottle.name.clone(), Arc::new(bottle));
        if runner != LOCAL && !forwards.is_empty() {
            self.forward(&runner, forwards);
        }
    }

    /// The bottle `name` released; whether there's none left to answer, and so folio serve's to stop.
    fn release(&self, name: &str) -> bool {
        let released = self
            .bottles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        let Some(released) = released else {
            return false;
        };
        log!("bottle.daemon", "{name}: released");
        let still = self
            .bottles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|b| b.runner == released.runner);
        if released.runner != LOCAL && !still {
            self.end_forward(&released.runner);
        }
        self.bottles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Forwards `forwards` to `runner`, unless that's under way already: each service's socket
    /// there to one here of that runner's own, so what comes by it is known to be from there.
    fn forward(self: &Arc<Self>, runner: &str, forwards: Vec<String>) {
        let mut all = self.forwards.lock().unwrap_or_else(|e| e.into_inner());
        if all.get(runner).is_some_and(|f| !f.thread.is_finished()) {
            return;
        }
        if let Some(old) = all.remove(runner) {
            self.close(&old);
        }
        log!("bottle.daemon", "forwarding to runner {runner}");
        let stop = Arc::new(AtomicBool::new(false));
        let mut sockets = Vec::new();
        let forwards: Vec<String> = forwards
            .iter()
            .filter_map(|pair| {
                let (there, here) = pair.split_once('=')?;
                let (name, service) = self
                    .routes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(Path::new(here))
                    .cloned()?;
                let socket = crate::daemon::socket_path(&format!("services/{runner}/{name}.sock"));
                match listen(&socket) {
                    Ok(listener) => {
                        self.accept(listener, service, runner, stop.clone());
                        sockets.push(socket.clone());
                        Some(format!("{there}={}", socket.display()))
                    }
                    Err(e) => {
                        log!("bottle.daemon", "{}: {e}", socket.display());
                        None
                    }
                }
            })
            .collect();
        let (told, name) = (stop.clone(), runner.to_string());
        let thread = thread::spawn(move || {
            if let Err(e) =
                crate::runner::forward_until(&name, &forwards, &|| told.load(Ordering::Relaxed))
            {
                log!("bottle.daemon", "forward to runner {name}: {e}");
            }
        });
        all.insert(
            runner.to_string(),
            Forward {
                stop,
                thread,
                sockets,
            },
        );
    }

    /// Takes the connections to `listener`, `service`'s for machines on `runner`, until `stop`, or
    /// folio serve's stopping.
    fn accept(
        self: &Arc<Self>,
        listener: UnixListener,
        service: Arc<dyn Service>,
        runner: &str,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        let (server, runner) = (self.clone(), runner.to_string());
        thread::spawn(move || {
            for stream in listener.incoming() {
                if server.stopping.load(Ordering::SeqCst) || stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(stream) = stream {
                    let (server, service, runner) =
                        (server.clone(), service.clone(), runner.clone());
                    thread::spawn(move || server.routed(service.as_ref(), stream, &runner));
                }
            }
        })
    }

    /// A forward's sockets, closed: their loops woken to see they're done, and gone.
    fn close(&self, forward: &Forward) {
        forward.stop.store(true, Ordering::Relaxed);
        for socket in &forward.sockets {
            let _ = UnixStream::connect(socket);
            let _ = fs::remove_file(socket);
        }
    }

    fn end_forward(&self, runner: &str) {
        let forward = self
            .forwards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(runner);
        if let Some(forward) = forward {
            log!("bottle.daemon", "no longer forwarding to runner {runner}");
            self.close(&forward);
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
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(e) => Reply::refused(format!("not a request: {e}")),
            Ok(message) => match message {
                Request::Ping => Reply {
                    code: Some(self.code.clone()),
                    ..Reply::ok()
                },
                Request::Bind(bind) => match Bottle::from_bind(*bind) {
                    Ok(bottle) => {
                        self.bind(bottle);
                        Reply::ok()
                    }
                    Err(e) => Reply::refused(e),
                },
                Request::Release { bottle } => {
                    let none_left = self.release(&bottle);
                    if none_left {
                        // Answered first, as a shutdown is: the wind-up may end this process
                        // before an answer after it is written.
                        let _ = (&stream).write_all(b"{\"ok\": true}\n");
                        log!("bottle.daemon", "no bottle left to answer: stopping");
                        self.stop();
                        return;
                    }
                    Reply::ok()
                }
                Request::Shutdown => {
                    log!("bottle.daemon", "shutdown requested");
                    // Answered first: the wind-up closes what comes after unanswered.
                    let _ = (&stream).write_all(b"{\"ok\": true}\n");
                    self.stop();
                    return;
                }
                Request::Listen { bottle } => return self.listen(&bottle, stream),
            },
        };
        let reply = serde_json::to_string(&reply).expect("a reply is JSON");
        let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
    }

    /// Forwards what `bottle` says to the snapshot address down `stream` until whoever listens
    /// closes it (or goes): nothing's kept of it after.
    fn listen(&self, bottle: &str, stream: UnixStream) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let Ok(writer) = stream.try_clone() else {
            return;
        };
        let _ = (&stream).write_all(b"{\"ok\": true}\n");
        self.listening
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(bottle.to_string())
            .or_default()
            .push((id, writer));
        log!("bottle.daemon", "{bottle}: listened to");
        // Nothing more comes from the listener: a read ends when it closes.
        let _ = stream.set_read_timeout(None);
        let _ = (&stream).read(&mut [0u8; 64]);
        let mut listening = self.listening.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(those) = listening.get_mut(bottle) {
            those.retain(|(i, _)| *i != id);
            if those.is_empty() {
                listening.remove(bottle);
            }
        }
    }

    /// A connection routed to `service`'s socket: answered by it as the bottle its header says it's
    /// from, or dropped; a line in the log for each: who, where, to what, how it ended, the bytes
    /// each way, and how long it took.
    fn routed(self: &Arc<Self>, service: &dyn Service, mut stream: UnixStream, runner: &str) {
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
        let Some(bottle) = header
            .machine
            .as_deref()
            .and_then(|m| self.bottle_of(m, runner))
        else {
            log!(
                "bottle.services",
                "services: refused {target} from {} on {runner}, which no bottle here is",
                header.machine.as_deref().unwrap_or("None")
            );
            return;
        };
        let started = Instant::now();
        let caller = Caller {
            bottle: &bottle,
            header: &header,
            bound: self.as_ref(),
        };
        let flow = service.serve(&caller, stream);
        let target = if flow.injected {
            format!("{target} +credential")
        } else {
            target
        };
        log!(
            "bottle.egress",
            "{} {target} -> {} {}{} up={} down={} {:.2}s",
            bottle.name,
            flow.to,
            flow.outcome,
            flow.status.map_or(String::new(), |s| format!(" {s}")),
            flow.up,
            flow.down,
            started.elapsed().as_secs_f64()
        );
    }
}

impl Bound for Server {
    fn bound(&self, name: &str) -> Option<Arc<Bottle>> {
        self.bottles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    fn all(&self) -> Vec<Arc<Bottle>> {
        let mut all: Vec<Arc<Bottle>> = self
            .bottles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        all
    }

    fn answer(&self, name: &str, text: &str) -> bool {
        let line = serde_json::to_string(&crate::control::Answer {
            answer: text.to_string(),
        })
        .expect("an answer is JSON")
            + "\n";
        let mut listening = self.listening.lock().unwrap_or_else(|e| e.into_inner());
        let Some(those) = listening.get_mut(name) else {
            return false;
        };
        // One gone without closing (a write fails) is no listener.
        those.retain(|(_, s)| (&*s).write_all(line.as_bytes()).is_ok());
        !those.is_empty()
    }
}

/// The service named `name` (the name of a --service), if there's one; `git`, the git service,
/// shared with bottle.host's (host.rs).
fn service_named(name: &str, git: &git::Git) -> Option<Box<dyn Service>> {
    Some(match name {
        "git" => Box::new(git.clone()),
        "host" => Box::new(host::Host { git: git.clone() }),
        "messages" => Box::new(messages::Messages),
        "bottles" => Box::new(bottles::Bottles),
        "ssh" => Box::new(ssh::Ssh),
        "credentials" => Box::new(credentials::Credentials),
        _ => return None,
    })
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
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    // None is unlimited.
    let soft = limit.current.unwrap_or(u64::MAX);
    raised(soft, limit.maximum, |soft| {
        let wanted = Rlimit {
            current: Some(soft),
            ..limit
        };
        setrlimit(Resource::Nofile, wanted).is_ok()
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
            eprintln!("folio: error: {e}");
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
        crate::store::private_dir(dir)?;
    }
    let server = Arc::new(Server {
        bottles: Mutex::new(HashMap::new()),
        listening: Mutex::new(HashMap::new()),
        forwards: Mutex::new(HashMap::new()),
        routes: Mutex::new(HashMap::new()),
        stopping: AtomicBool::new(false),
        done: AtomicBool::new(false),
        control: control.clone(),
        services: services.clone(),
        code: crate::control::code(),
    });
    idle::watch(server.clone());
    let mut routed = Vec::new();
    let git = git::Git::default();
    for (name, path) in &services {
        let service: Arc<dyn Service> = service_named(name, &git)
            .ok_or_else(|| io::Error::other(format!("no service {name}")))?
            .into();
        let listener = listen(path)?;
        server
            .routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.clone(), (name.clone(), service.clone()));
        routed.push(server.accept(listener, service, LOCAL, Arc::new(AtomicBool::new(false))));
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
        let bottles: Vec<Request> = serde_json::from_str(given.trim()).unwrap_or_default();
        for message in bottles {
            let Request::Bind(bind) = message else {
                continue;
            };
            let name = bind.bottle.clone();
            match Bottle::from_bind(*bind) {
                Ok(bottle) => server.bind(bottle),
                Err(e) => log!(
                    "bottle.daemon",
                    "{name}: couldn't restore its decisions: {e}"
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
                let runners: Vec<String> = server
                    .forwards
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys()
                    .cloned()
                    .collect();
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
