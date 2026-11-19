//! `runner add|ls|rm|check|detach|attach|home|ssh|forward`.

use std::io::Write;
use std::os::unix::process::CommandExt;

use std::io;
use std::process::{ExitCode, Stdio};

use crate::FAILED;

use super::forward::forward;
use super::ssh::Runner;
use super::{Entry, USAGE, home, other, registered, registry, write_registry};

/// Runs `runner COMMAND ...` with `args`; `bottles_on` says which of folio's bottles are on a
/// runner, by name, and `pause_on` pauses the threads running there, asking their agents first:
/// their names.
pub fn main(
    mut args: impl Iterator<Item = String>,
    bottles_on: &dyn Fn(&str) -> Result<Vec<String>, String>,
    pause_on: &dyn Fn(&str) -> Result<Vec<String>, String>,
) -> ExitCode {
    let command = args.next();
    let args: Vec<String> = args.collect();
    let result = match (command.as_deref(), args.as_slice()) {
        (Some("add"), [name, flag, destination]) if flag == "--ssh" => add(name, destination),
        (Some("ls" | "list"), []) => ls(false),
        (Some("ls" | "list"), [flag]) if flag == "--json" => ls(true),
        (Some("rm"), [name]) => rm(name, bottles_on),
        (Some("check"), [target]) => check(target, false),
        (Some("check"), [target, flag]) if flag == "--json" => check(target, true),
        (Some("detach"), [name]) => detach(name, false, pause_on),
        (Some("detach"), [name, flag]) if flag == "--offline" => detach(name, true, pause_on),
        (Some("attach"), [name]) => attach(name),
        (Some("home"), [name]) => home(name).map(|home| println!("{home}")),
        (Some("forward"), [name, forwards @ ..]) if !forwards.is_empty() => forward(name, forwards),
        (Some("ssh"), [name, command @ ..]) => {
            return match Runner::named(name) {
                // Its stdin, stdout and terminal this process's: git's, say.
                Ok(runner) => {
                    let mut ssh = runner.ssh(None);
                    ssh.args(command);
                    let e = ssh.exec();
                    eprintln!("folio: error: ssh: {e}");
                    ExitCode::from(FAILED)
                }
                Err(e) => {
                    eprintln!("folio: error: {e}");
                    ExitCode::from(FAILED)
                }
            };
        }
        _ => return crate::usage_error(USAGE),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // As every folio command fails (but `runner ssh`, which is ssh, and `on`, which is
        // bottle-cli: theirs).
        Err(e) => {
            eprintln!("folio: error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Checks a machine, copies bottle-cli there, and adds it as runner `name`.
pub(super) fn add(name: &str, destination: &str) -> io::Result<()> {
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
            "runner {name} is already {}; `folio runner rm {name}` first",
            known.ssh
        )));
    }
    let runner = Runner::new(name, destination);
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

pub(super) fn ls(json: bool) -> io::Result<()> {
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

/// Marks runner `name` detached, having stopped `detached` (Some), or attached (None).
pub(super) fn mark(name: &str, detached: Option<&[String]>) -> io::Result<()> {
    let mut registry = registry()?;
    let entry = registry
        .runners
        .get_mut(name)
        .ok_or_else(|| other(format!("no runner named {name}")))?;
    match detached {
        Some(stopped) => {
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            entry.rest.insert(
                "detached".into(),
                serde_json::json!({"at": at, "stopped": stopped}),
            );
        }
        None => {
            entry.rest.remove("detached");
        }
    }
    write_registry(&registry)
}

/// The machines running on `runner`, by their names there.
pub(super) fn running(runner: &Runner) -> io::Result<Vec<String>> {
    let output = runner
        .cli(&["ls".into(), "--json".into()], Some(false))
        .stdin(Stdio::null())
        .output()?;
    runner.reached(&output)?;
    if !output.status.success() {
        return Err(other(String::from_utf8_lossy(&output.stderr).trim()));
    }
    let listed: Vec<bottle_formats::listing::Machine> =
        serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;
    Ok(listed
        .into_iter()
        .filter(|m| m.state == bottle_formats::listing::State::Running)
        .map(|m| m.name)
        .collect())
}

/// `runner detach NAME [--offline]` (see the module's doc).
pub(super) fn detach(
    name: &str,
    offline: bool,
    pause_on: &dyn Fn(&str) -> Result<Vec<String>, String>,
) -> io::Result<()> {
    let entry = registered()?
        .remove(name)
        .ok_or_else(|| other(format!("no runner named {name}")))?;
    if entry.detached() {
        println!("Runner {name} is detached already: it's safe to turn off");
        return Ok(());
    }
    let mut stopped = Vec::new();
    if !offline {
        let runner = Runner::new(name, entry.ssh);
        let off = |e: io::Error| {
            other(format!(
                "{e}; if it's off already, `folio runner detach {name} --offline`"
            ))
        };
        runner.ensure_dist().map_err(off)?;
        let up = running(&runner).map_err(off)?;
        if !up.is_empty() {
            if !ask(&format!(
                "Running on {name}: {}. Detaching pauses the threads among them, their snapshots \
                 coming here, each one's agent asked to get ready first (the pause waits for its \
                 reply), and stops the rest, ending whatever runs in them.\nPause and stop them now? \
                 [y/N] ",
                up.join(", ")
            )) {
                return Err(other(format!(
                    "runner {name} is still attached, and {} still running: `folio runner detach \
                     {name}` again when they can be stopped",
                    up.join(", ")
                )));
            }
            // A thread not paused (Ctrl-C, say) leaves the runner attached, and the rest running.
            let paused = pause_on(name)
                .map_err(|e| other(format!("{e}; runner {name} is still attached")))?;
            for thread in &paused {
                println!("Paused thread {thread}");
            }
            let up = running(&runner).map_err(off)?;
            for machine in &up {
                let out = runner
                    .cli(&["stop".into(), machine.clone()], Some(false))
                    .stdin(Stdio::null())
                    .output()?;
                runner.reached(&out)?;
                if !out.status.success() {
                    return Err(other(format!("couldn't stop {machine} on {name}")));
                }
                println!("Stopped {machine}");
                stopped.push(machine.clone());
            }
            let still = running(&runner).map_err(off)?;
            if !still.is_empty() {
                return Err(other(format!(
                    "runner {name} still runs {}: `folio runner detach {name}` again",
                    still.join(", ")
                )));
            }
        }
    }
    mark(name, Some(&stopped))?;
    if offline {
        println!("Runner {name} detached, unasked: nothing was stopped or checked");
    } else {
        println!("Runner {name} detached: nothing runs on it, so it's safe to turn off");
    }
    Ok(())
}

/// Whether the user, asked `question` on a terminal, says yes; no, without one.
pub(super) fn ask(question: &str) -> bool {
    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        return false;
    }
    print!("{question}");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// `runner attach NAME`: attached again, once it answers.
pub(super) fn attach(name: &str) -> io::Result<()> {
    let entry = registered()?
        .remove(name)
        .ok_or_else(|| other(format!("no runner named {name}")))?;
    let stopped: Vec<String> = entry
        .rest
        .get("detached")
        .and_then(|d| d["stopped"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.as_str().map(String::from))
        .collect();
    Runner::new(name, entry.ssh).found()?;
    mark(name, None)?;
    println!("Runner {name} attached");
    if !stopped.is_empty() {
        println!(
            "Running when it was detached: {}. Their threads' bottles start again when they're next used.",
            stopped.join(", ")
        );
    }
    Ok(())
}

/// What runner `target` (a name), or the machine at DESTINATION `target`, is and lacks.
pub(super) fn check(target: &str, json: bool) -> io::Result<()> {
    let runner = Runner::named(target).unwrap_or_else(|_| Runner::new(target, target));
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

/// Forgets runner `name`. Nothing there is touched: what's there of folio's bottles is folio's to
/// have deleted first.
/// `runner rm NAME`: forgotten, but not while folio has bottles there (their threads are deleted
/// first).
pub(super) fn rm(
    name: &str,
    bottles_on: &dyn Fn(&str) -> Result<Vec<String>, String>,
) -> io::Result<()> {
    let there = bottles_on(name).map_err(other)?;
    if !there.is_empty() {
        return Err(other(format!(
            "runner {name} has bottles: {}; delete their threads first",
            there.join(", ")
        )));
    }
    let mut registry = registry()?;
    if registry.runners.remove(name).is_none() {
        return Err(other(format!("no runner named {name}")));
    }
    write_registry(&registry)
}
