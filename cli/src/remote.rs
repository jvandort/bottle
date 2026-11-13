//! A runner's machines, as this machine has them: `RUNNER/NAME` (see runner.rs).
//!
//! What's asked of one is asked of this program on the runner, over its shared ssh connection,
//! the name its own there: its output, exit status and terminal are this process's. But for what a
//! machine's network policy routes to unix sockets: those sockets are this machine's (bottled's,
//! folio's), so creating one rewrites each route's socket to one on the runner, kept here with the
//! socket it stands for ($BOTTLE_HOME/krun/runners/RUNNER/NAME/forwards.json), and starting one
//! starts, here, a process of its own that forwards those over ssh while the machine runs
//! (`bottle-cli forward RUNNER/NAME`, logged beside them): connected again whenever ssh drops,
//! waiting longer each time up to a minute, and ending when the machine does. A machine stopped
//! there is started from here (by `start`, `exec` or `shell`), so its forwards come with it.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::FAILED;
use crate::runner::{self, Runner};

/// How often a forward checks its machine still runs; the longest it waits to connect again.
const CHECK: Duration = Duration::from_secs(15);
const MOST_BACKOFF: Duration = Duration::from_secs(60);

/// Where this machine keeps what it knows of `runner`'s machine `name`.
fn record(runner: &str, name: &str) -> PathBuf {
    runner::home().join("krun/runners").join(runner).join(name)
}

/// The names this machine knows of machines on `runner`.
pub fn records(runner: &str) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(runner::home().join("krun/runners").join(runner)) {
        for entry in entries.flatten() {
            names.extend(entry.file_name().to_str().map(String::from));
        }
    }
    names.sort();
    Ok(names)
}

/// Pairs of a socket on the runner and the one here it stands for.
type Forwards = Vec<(String, String)>;

fn forwards(runner: &str, name: &str) -> Forwards {
    fs::read(record(runner, name).join("forwards.json"))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

/// `COMMAND RUNNER/NAME ARGS`, a machine command for a runner's machine: this program's, there,
/// by the machine's name there.
pub fn machine(command: &str, qualified: &str, args: &[String]) -> ExitCode {
    let Some((runner, name)) = runner::split(qualified) else {
        return crate::usage_error(&format!("{qualified}: not RUNNER/NAME"));
    };
    let (runner, name) = (runner.to_string(), name.to_string());
    let mut there = args.to_vec();
    report((|| -> io::Result<ExitCode> {
        let runner = Runner::named(&runner)?;
        // Whatever's asked, of this program's own build there: one older may not know the command
        // (a delete or a stop, with nothing created or started there since).
        runner.ensure_dist()?;
        match command {
            "create" | "configure" => {
                let policy = give_network(&runner, &name, &mut there)?;
                let code = pass(&runner, command, &name, &there, policy.as_deref())?;
                if code != ExitCode::SUCCESS {
                    let _ = fs::remove_dir_all(record(&runner.name, &name));
                }
                Ok(code)
            }
            "start" => start(&runner, &name),
            "stop" => {
                let code = pass(&runner, command, &name, &there, None)?;
                end_forward(&runner.name, &name);
                Ok(code)
            }
            "delete" => {
                let code = pass(&runner, command, &name, &there, None)?;
                if code == ExitCode::SUCCESS {
                    end_forward(&runner.name, &name);
                    let _ = fs::remove_dir_all(record(&runner.name, &name));
                }
                Ok(code)
            }
            "rename" => {
                let to = args
                    .first()
                    .ok_or_else(|| io::Error::other("rename takes the machine's new name"))?;
                let to = runner::split(to)
                    .map_or(to.as_str(), |(_, n)| n)
                    .to_string();
                there[0] = to.clone();
                let code = pass(&runner, command, &name, &there, None)?;
                if code == ExitCode::SUCCESS && record(&runner.name, &name).exists() {
                    fs::rename(record(&runner.name, &name), record(&runner.name, &to))?;
                }
                Ok(code)
            }
            "exec" => {
                // Stopped, it's started from here, its forwards with it.
                if state(&runner, &name)?.as_deref() == Some("stopped") {
                    let started = start(&runner, &name)?;
                    if started != ExitCode::SUCCESS {
                        return Ok(started);
                    }
                }
                // Attached wholly, a terminal and all: this process becomes the ssh.
                let tty = there.iter().any(|a| a == "-t");
                let full: Vec<String> = ["exec".to_string(), name.clone()]
                    .into_iter()
                    .chain(there)
                    .collect();
                Err(runner.cli(&full, Some(tty)).exec())
            }
            "shell" => Ok(machine(
                "exec",
                qualified,
                &crate::machine::LOGIN.map(String::from),
            )),
            _ => pass(&runner, command, &name, &there, None),
        }
    })())
}

fn report(result: io::Result<ExitCode>) -> ExitCode {
    result.unwrap_or_else(|e| {
        eprintln!("bottle-cli: {e}");
        ExitCode::from(FAILED)
    })
}

/// `COMMAND NAME ARGS` there, `input` its stdin (else this process's): its exit status.
fn pass(
    runner: &Runner,
    command: &str,
    name: &str,
    args: &[String],
    input: Option<&[u8]>,
) -> io::Result<ExitCode> {
    let full: Vec<String> = [command.to_string(), name.to_string()]
        .into_iter()
        .chain(args.iter().cloned())
        .collect();
    let mut child = runner
        .cli(&full, Some(false))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(input) = input {
        io::Write::write_all(&mut child.stdin.take().unwrap(), input)?;
    }
    let output = child.wait_with_output()?;
    io::Write::write_all(&mut io::stderr(), &output.stderr)?;
    runner.reached(&output)?;
    Ok(ExitCode::from(output.status.code().unwrap_or(1) as u8))
}

/// The policy `args` give (--network FILE|-), its routes' sockets rewritten to ones on the runner,
/// to give there on stdin (`args` saying so); what they stand for kept here.
fn give_network(runner: &Runner, name: &str, args: &mut [String]) -> io::Result<Option<Vec<u8>>> {
    let Some(at) = args.iter().position(|a| a == "--network") else {
        return Ok(None);
    };
    let given = args
        .get(at + 1)
        .ok_or_else(|| io::Error::other("--network needs FILE"))?;
    let json = crate::boot::policy_given(given)?;
    let mut policy: serde_json::Value = serde_json::from_slice(&json).map_err(io::Error::other)?;
    let home = runner.home()?;
    // Short, as a unix socket's path must be: named for the machine and the socket here.
    let mut forwards: Forwards = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(routes) = policy.get_mut("routes").and_then(|r| r.as_array_mut()) {
        for route in routes {
            let Some(here) = route
                .get("socket")
                .and_then(|s| s.as_str())
                .map(String::from)
            else {
                continue;
            };
            use sha2::Digest;
            let hash = sha2::Sha256::digest(format!("{name}\0{here}"));
            let key: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
            let there = format!("{home}/.bottle/routes/{key}.sock");
            if seen.insert(here.clone()) {
                forwards.push((there.clone(), here));
            }
            route["socket"] = there.into();
        }
    }
    let dir = record(&runner.name, name);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("forwards.json"),
        serde_json::to_vec_pretty(&forwards).unwrap(),
    )?;
    args[at + 1] = "-".into();
    Ok(Some(serde_json::to_vec(&policy).unwrap()))
}

/// `name`'s state there; None if there's no such machine.
fn state(runner: &Runner, name: &str) -> io::Result<Option<String>> {
    let args = ["status", name, "--json"].map(String::from);
    let output = runner
        .cli(&args, Some(false))
        .stdin(Stdio::null())
        .output()?;
    runner.reached(&output)?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        if said.contains("no machine ") {
            return Ok(None);
        }
        return Err(io::Error::other(said.trim().to_string()));
    }
    let status: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;
    Ok(status["state"].as_str().map(String::from))
}

/// Starts `name` there, and here the process that forwards its routes while it runs.
fn start(runner: &Runner, name: &str) -> io::Result<ExitCode> {
    let code = pass(runner, "start", name, &[], None)?;
    if code == ExitCode::SUCCESS && !forwards(&runner.name, name).is_empty() {
        begin_forward(&runner.name, name)?;
    }
    Ok(code)
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only asks whether the process is there.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

fn forward_pid(runner: &str, name: &str) -> Option<i32> {
    fs::read_to_string(record(runner, name).join("forward.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The forward of `name`'s routes, started here in the background, unless it's running.
fn begin_forward(runner: &str, name: &str) -> io::Result<()> {
    if forward_pid(runner, name).is_some_and(alive) {
        return Ok(());
    }
    let dir = record(runner, name);
    let log = fs::File::create(dir.join("forward.log"))?;
    let child = Command::new(std::env::current_exe()?)
        .args(["forward", &format!("{runner}/{name}")])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Its own session: it outlives this command, and this terminal. In /, not this command's directory,
        // which may be deleted while it runs.
        .process_group(0)
        .current_dir("/")
        .spawn()?;
    fs::write(dir.join("forward.pid"), child.id().to_string())
}

fn end_forward(runner: &str, name: &str) {
    if let Some(pid) = forward_pid(runner, name).filter(|&p| alive(p)) {
        // Its process group, its own (see begin_forward): its ssh with it.
        // SAFETY: kill with a process group's id.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
}

/// `bottle-cli forward RUNNER/NAME`: forwards the machine's routes over ssh while it runs there.
pub fn forward(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some((runner, name)) = args
        .next()
        .and_then(|a| runner::split(&a).map(|(r, n)| (r.to_string(), n.to_string())))
    else {
        return crate::usage_error("forward needs RUNNER/NAME");
    };
    report((|| -> io::Result<ExitCode> {
        let runner = Runner::named(&runner)?;
        let forwards = forwards(&runner.name, &name);
        let mut backoff = Duration::from_secs(1);
        loop {
            match state(&runner, &name) {
                Ok(Some(s)) if s == "running" => {}
                Ok(_) => {
                    eprintln!(
                        "bottle-cli: {}/{name} isn't running: forward ended",
                        runner.name
                    );
                    return Ok(ExitCode::SUCCESS);
                }
                Err(e) => {
                    // Off, or unreachable for now: tried again, as if the forward had dropped.
                    eprintln!("bottle-cli: {e}");
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(MOST_BACKOFF);
                    continue;
                }
            }
            let since = Instant::now();
            match connect(&runner, &name, &forwards) {
                Ok(Ended::Stopped) => return Ok(ExitCode::SUCCESS),
                Ok(Ended::Dropped(why)) => eprintln!("bottle-cli: forward dropped: {why}"),
                Err(e) => eprintln!("bottle-cli: forward: {e}"),
            }
            // One that held a while starts over: a drop isn't a failure to connect.
            if since.elapsed() > MOST_BACKOFF {
                backoff = Duration::from_secs(1);
            }
            thread::sleep(backoff);
            backoff = (backoff * 2).min(MOST_BACKOFF);
        }
    })())
}

enum Ended {
    Stopped,
    Dropped(String),
}

/// One ssh forwarding `forwards`, until it drops or the machine stops.
fn connect(runner: &Runner, name: &str, forwards: &Forwards) -> io::Result<Ended> {
    // Whatever a forward that died left there, gone first: ssh won't bind over it.
    let theres: Vec<String> = forwards.iter().map(|(t, _)| runner::quote(t)).collect();
    let made = runner.run(
        &format!(
            r#"mkdir -p "$HOME/.bottle/routes" && rm -f {}"#,
            theres.join(" ")
        ),
        None,
    )?;
    if !made.status.success() {
        return Ok(Ended::Dropped(
            String::from_utf8_lossy(&made.stderr).trim().to_string(),
        ));
    }
    let mut ssh = Command::new(std::env::var_os("BOTTLE_SSH").unwrap_or("ssh".into()));
    ssh.args(["-N", "-T"]);
    for option in [
        "BatchMode=yes",
        "ConnectTimeout=10",
        "ExitOnForwardFailure=yes",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        // A connection of its own, not the shared one: it lives as long as the forward does.
        "ControlMaster=no",
        "ControlPath=none",
    ] {
        ssh.args(["-o", option]);
    }
    for (there, here) in forwards {
        ssh.args(["-R", &format!("{there}:{here}")]);
    }
    ssh.args(["--", &runner.destination])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = ssh.spawn()?;
    eprintln!("bottle-cli: forwarding {}/{name}'s routes", runner.name);
    loop {
        let waited = Instant::now();
        while waited.elapsed() < CHECK {
            if let Some(status) = child.try_wait()? {
                let mut said = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut said);
                }
                let why = said
                    .lines()
                    .last()
                    .map_or(format!("ssh exited: {status}"), String::from);
                return Ok(Ended::Dropped(why));
            }
            thread::sleep(Duration::from_millis(200));
        }
        if let Ok(state) = state(runner, name)
            && state.as_deref() != Some("running")
        {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("bottle-cli: {}/{name} stopped: forward ended", runner.name);
            return Ok(Ended::Stopped);
        }
    }
}

/// Every machine on every attached runner, as `ls --json` there has them, named `RUNNER/NAME`; a
/// runner that can't be asked is said on stderr.
pub fn listed() -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    let Ok(runners) = runner::registered() else {
        return rows;
    };
    for (name, entry) in runners {
        if entry.detached() {
            // Not asked: what's known of its machines here.
            for machine in records(&name).unwrap_or_default() {
                rows.push(
                    serde_json::json!({"name": format!("{name}/{machine}"), "state": "detached"}),
                );
            }
            continue;
        }
        let runner = Runner {
            name: name.clone(),
            destination: entry.ssh,
        };
        let asked = runner
            .cli(&["ls".into(), "--json".into()], Some(false))
            .stdin(Stdio::null())
            .output();
        let listed = asked.and_then(|output| {
            runner.reached(&output)?;
            if !output.status.success() {
                return Err(io::Error::other(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ));
            }
            serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
                .map_err(io::Error::other)
        });
        match listed {
            Ok(machines) => {
                for mut row in machines {
                    let local = row["name"].as_str().unwrap_or_default().to_string();
                    row["name"] = format!("{name}/{local}").into();
                    rows.push(row);
                }
            }
            Err(e) => eprintln!("bottle-cli: runner {name}: {e}"),
        }
    }
    rows
}
