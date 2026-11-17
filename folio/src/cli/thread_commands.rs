//! The thread commands: new, claude/shell, exec, cp, pause, notes, broker, rename, list, delete.

use std::io::{IsTerminal, Write};
use std::path::Path;

use clap::ArgMatches;

use crate::bottles::State;
use crate::engine::Engine;
use crate::repos::Repo;
use crate::threads::{self, Settings, Thread};
use crate::{broker, notes, obsidian, picker, prompt};

use super::output::{ago, human_size, table};
use super::{Outcome, arg, args, current, flag};

pub(super) fn exec(engine: &Engine, matches: &ArgMatches, sidecar: bool, command: &str) -> Outcome {
    let repo = current()?;
    let mut argv = args(matches, "argv");
    if argv.first().map(String::as_str) == Some("--") {
        argv.remove(0);
    }
    if argv.is_empty() {
        return Err(format!(
            "give the command to run: `folio {command} THREAD -- COMMAND`"
        ));
    }
    let thread = threads::load(&repo, arg(matches, "thread").unwrap())?;
    match threads::exec(engine, &repo, &thread, &argv, sidecar, flag(matches, "tty"))? {}
}

pub(super) fn cp(engine: &Engine, matches: &ArgMatches, sidecar: bool, command: &str) -> Outcome {
    let repo = current()?;
    // THREAD:PATH, if what's before the first colon is a thread of the repo's; else a path here.
    let side = |given: &str| -> Result<(Option<Thread>, String), String> {
        if let Some((name, path)) = given.split_once(':')
            && threads::exists(&repo, name)
        {
            return Ok((
                Some(threads::load(&repo, name)?),
                if path.is_empty() {
                    ".".into()
                } else {
                    path.into()
                },
            ));
        }
        Ok((None, given.to_string()))
    };
    let (src, dst) = (
        side(arg(matches, "src").unwrap())?,
        side(arg(matches, "dst").unwrap())?,
    );
    if src.0.is_none() && dst.0.is_none() {
        return Err(format!(
            "`folio {command}` copies into or out of a thread's {}: name one, THREAD:PATH",
            if sidecar { "sidecar" } else { "bottle" }
        ));
    }
    let went = threads::copy(
        engine,
        &repo,
        (src.0.as_ref(), &src.1),
        (dst.0.as_ref(), &dst.1),
        sidecar,
    )?;
    match &dst.0 {
        Some(thread) => println!("{}:{went}", thread.name),
        None => println!("{went}"),
    }
    Ok(0)
}

pub(super) fn new(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let thread = start(
        engine,
        &current()?,
        arg(matches, "thread").unwrap(),
        arg(matches, "from"),
        arg(matches, "runner"),
    )?;
    println!(
        "Work in it with: folio claude {name}  (or folio shell {name})",
        name = thread.name
    );
    Ok(0)
}

pub(super) fn start(
    engine: &Engine,
    repo: &Repo,
    name: &str,
    base: Option<&str>,
    runner: Option<&str>,
) -> Result<Thread, String> {
    crate::log::say!(
        "threads",
        "Starting thread {name} in {}...",
        repo.path.display()
    );
    let thread = threads::create(engine, repo, name, base, false, runner)?;
    println!(
        "Started thread {name} in bottle {}. Its branches: git fetch bottle/{name}",
        thread.bottle,
        name = thread.name
    );
    // Flushed: attaching next replaces this process, and with it anything still buffered.
    let _ = std::io::stdout().flush();
    Ok(thread)
}

pub(super) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The thread the command names (started, if it's new and the command may start one), else the
/// one chosen in the picker; None if none was.
pub(super) fn thread_of(
    engine: &Engine,
    matches: &ArgMatches,
    repo: &Repo,
    command: &str,
    new: bool,
) -> Result<Option<Thread>, String> {
    if let Some(name) = arg(matches, "thread") {
        if new && !threads::exists(repo, name) {
            let runner = matches
                .try_get_one::<String>("runner")
                .ok()
                .flatten()
                .map(String::as_str);
            return start(engine, repo, name, None, runner).map(Some);
        }
        return threads::load(repo, name).map(Some);
    }
    if !interactive() {
        return Err(format!(
            "give the thread: `folio {command} THREAD` (choosing one needs a terminal)"
        ));
    }
    let all = threads::all(repo)?;
    if all.is_empty() && !new {
        return Err(format!(
            "{} has no threads; start one with `folio new THREAD`",
            repo.path.display()
        ));
    }
    let states = threads::states(engine, repo, &all)?;
    let all = threads::recent(repo, all, &states);
    let width = all
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut choices = if new {
        vec![picker::new_thread()]
    } else {
        Vec::new()
    };
    for t in &all {
        let row = &states[&t.name];
        choices.push(picker::Choice {
            label: format!(
                "{:<width$}  {:<8}  {}",
                t.name,
                row.status.state.as_str(),
                ago(threads::last_active(repo, t, row))
            ),
            thread: Some(t.name.clone()),
        });
    }
    let Some(choice) = picker::pick(
        &format!("folio {command}: choose a thread (j/k, Enter; q to leave)"),
        choices,
    )?
    else {
        return Ok(None);
    };
    if let Some(name) = choice.thread {
        return threads::load(repo, &name).map(Some);
    }
    match prompt::input("The new thread's name: ").map(|n| n.trim().to_string()) {
        Some(name) if !name.is_empty() => start(engine, repo, &name, None, None).map(Some),
        _ => Ok(None),
    }
}

pub(super) fn attach(
    engine: &Engine,
    matches: &ArgMatches,
    session: &str,
    command: &str,
) -> Outcome {
    let repo = current()?;
    let Some(mut thread) = thread_of(engine, matches, &repo, command, true)? else {
        return Ok(0);
    };
    if let Some(runner) = arg(matches, "runner") {
        let settings = Settings {
            runner: Some(runner.into()),
            ..thread.settings.clone()
        };
        thread = threads::set_settings(engine, &repo, &thread, settings)?;
    }
    match threads::attach(engine, &repo, &thread, session)? {}
}

pub(super) fn pause(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(engine, matches, &repo, "pause", false)? else {
        return Ok(0);
    };
    crate::log::say!("threads", "Pausing thread {}...", thread.name);
    threads::pause(engine, &repo, &thread)?;
    println!(
        "Paused thread {name}. Resume it with: folio claude {name}",
        name = thread.name
    );
    Ok(0)
}

pub(super) fn broker_command(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let command = arg(matches, "command").unwrap_or("claude");
    let identity = broker::repo();
    let mut thread = if threads::exists(&identity, broker::NAME) {
        Some(threads::load(&identity, broker::NAME)?)
    } else {
        None
    };
    let notes_path = broker::notes_path(&identity);
    if command == "notes" {
        return open_notes(&notes::ensure(&notes_path, "The broker's notes")?);
    }
    if thread.is_none() && command == "pause" {
        return Err("there's no broker to pause; `folio broker` starts it".into());
    }
    if thread.is_none() {
        crate::log::say!("threads", "Starting the broker...");
        thread = Some(threads::create(
            engine,
            &identity,
            broker::NAME,
            None,
            true,
            None,
        )?);
        println!("Started the broker. Its notes: {}", notes_path.display());
        let _ = std::io::stdout().flush();
    }
    let thread = thread.unwrap();
    if command == "pause" {
        crate::log::say!("threads", "Pausing the broker...");
        threads::pause(engine, &identity, &thread)?;
        println!("Paused the broker. Resume it with: folio broker");
        return Ok(0);
    }
    let session = if command == "claude" {
        threads::CLAUDE
    } else {
        threads::SHELL
    };
    match threads::attach(engine, &identity, &thread, session)? {}
}

/// Open the vault in Obsidian, if it's installed; else say where it is.
pub(super) fn open_notes(path: &Path) -> Outcome {
    if obsidian::installed() {
        println!("{}", obsidian::open_vault(path)?);
    } else {
        println!(
            "The notes are in {}: open that folder as a vault in Obsidian.",
            path.display()
        );
    }
    Ok(0)
}

pub(super) fn notes_command(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let name = match arg(matches, "thread") {
        Some(name) => name.to_string(), // a deleted thread's notes too
        None => match thread_of(engine, matches, &repo, "notes", false)? {
            Some(thread) => thread.name,
            None => return Ok(0),
        },
    };
    let path = threads::notes_dir(&repo, &name);
    if !path.exists() {
        return Err(format!(
            "thread {name} has no notes yet: its bottle starts them"
        ));
    }
    open_notes(&path)
}

pub(super) fn rename(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let old = arg(matches, "thread").unwrap();
    let thread = threads::rename(
        engine,
        &repo,
        &threads::load(&repo, old)?,
        arg(matches, "new").unwrap(),
    )?;
    println!(
        "Renamed thread {old} to {name}. Its branches: git fetch bottle/{name}; resume it with: folio claude {name}",
        name = thread.name
    );
    Ok(0)
}

pub(super) fn list(engine: &Engine) -> Outcome {
    let repo = current()?;
    let all = threads::all(&repo)?;
    let states = threads::states(engine, &repo, &all)?;
    let all = threads::recent(&repo, all, &states);
    let mut rows = vec![
        [
            "THREAD", "STATE", "RUNNER", "SIDECAR", "SNAPSHOT", "ACTIVE", "FEATURES",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for thread in &all {
        let row = &states[&thread.name];
        let state = row.status.state;
        let size = if state == State::Paused {
            threads::snapshot_size(&repo, thread)
        } else {
            None
        };
        // Where its bottle is; for one without, where its next is made.
        let bottle = row.status.bottle.as_ref();
        let runner = match bottle.map(|b| b.runner.as_str()).filter(|r| !r.is_empty()) {
            Some(runner) => runner.to_string(),
            None => threads::runner_of(&repo, thread)?.unwrap_or_else(|| "local".into()),
        };
        // Its sidecar's runner, and how it is, unless there's none to speak of (a paused thread's).
        let sidecar = match &thread.sidecar {
            None => "-".to_string(),
            Some(s) => {
                let runner = s.runner.clone().unwrap_or_default();
                match row.sidecar {
                    Some(State::Missing) | None => runner,
                    Some(state) => format!("{runner} ({state})"),
                }
            }
        };
        let mut features: Vec<&str> = bottle
            .into_iter()
            .flat_map(|b| &b.features)
            .map(String::as_str)
            .collect();
        features.sort();
        rows.push(vec![
            thread.name.clone(),
            state.to_string(),
            runner,
            sidecar,
            size.map_or("-".into(), human_size),
            ago(threads::last_active(&repo, thread, row)),
            if features.is_empty() {
                "-".into()
            } else {
                features.join(" ")
            },
        ]);
    }
    table(&rows);
    Ok(0)
}

pub(super) fn delete(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(engine, matches, &repo, "delete", false)? else {
        return Ok(0);
    };
    if !flag(matches, "yes") {
        if !std::io::stdin().is_terminal() {
            return Err(format!(
                "deleting {} needs --yes when there's no terminal to ask on",
                thread.name
            ));
        }
        let lost = threads::losses(engine, &repo, &thread)?;
        if lost.is_empty() {
            println!(
                "Deleting {} loses nothing that isn't on your branches or origin's.",
                thread.name
            );
        } else {
            println!("Deleting {} loses:", thread.name);
            for item in &lost {
                println!("  {item}");
            }
        }
        let typed = prompt::input(&format!("Type {} to delete it: ", thread.name))
            .map(|t| t.trim().to_string());
        if typed.as_deref() != Some(thread.name.as_str()) {
            println!("Not deleted.");
            return Ok(1);
        }
    }
    threads::delete(engine, &repo, &thread)?;
    println!("Deleted thread {}", thread.name);
    Ok(0)
}
