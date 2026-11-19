//! What isn't a thread's: runners, folio serve, credentials, repos.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::ArgMatches;

use crate::{auth, bottles, broker, daemon, features, prompt, repos, threads};

use super::output::table;
use super::{Outcome, arg, args, flag};

/// folio's runner commands: runner.rs's.
pub(super) fn runner_command(matches: &ArgMatches) -> ExitCode {
    crate::runner::main(
        args(matches, "arguments").into_iter(),
        &bottles_on,
        &pause_on,
        &threads::clear_left,
    )
}

/// Pause every thread running on runner `name`, of every repo folio knows and the broker's, each
/// asking its agent first, all at once: their names. One that fails is said, and fails the whole,
/// once all are done.
pub(super) fn pause_on(name: &str) -> Result<Vec<String>, String> {
    let there = bottles_on(name)?;
    let mut running = Vec::new();
    for repo in repos::known().into_iter().chain([broker::repo()]) {
        let on: Vec<threads::Thread> = threads::all(&repo)?
            .into_iter()
            .filter(|t| there.contains(&t.bottle))
            .collect();
        let states = threads::states(&crate::engine::Engine::default(), &repo, &on)?;
        running.extend(
            on.into_iter()
                .filter(|t| states[&t.name].status.state == bottles::State::Running)
                .map(|t| (repo.clone(), t)),
        );
    }
    let paused: Vec<Result<String, String>> = std::thread::scope(|scope| {
        let pausing: Vec<_> = running
            .iter()
            .map(|(repo, thread)| {
                scope.spawn(move || {
                    // An engine of its own: one keeps what it learns in cells, for one thread.
                    let engine = crate::engine::Engine::default();
                    threads::pause(&engine, repo, thread, true)
                        .map(|()| thread.name.clone())
                        .map_err(|e| format!("couldn't pause thread {}: {e}", thread.name))
                })
            })
            .collect();
        pausing.into_iter().map(|p| p.join().unwrap()).collect()
    });
    let failed: Vec<String> = paused.iter().filter_map(|p| p.clone().err()).collect();
    if !failed.is_empty() {
        return Err(failed.join("; "));
    }
    Ok(paused.into_iter().filter_map(Result::ok).collect())
}

/// The bottles on runner `name`, by name: what `runner rm` refuses to forget a runner while it has.
pub(super) fn bottles_on(name: &str) -> Result<Vec<String>, String> {
    let mut there: Vec<String> = bottles::load()?
        .into_values()
        .filter(|b| b.runner == name)
        .map(|b| b.name)
        .collect();
    there.sort();
    Ok(there)
}

pub(super) fn serve_start(matches: &ArgMatches) -> Outcome {
    if !flag(matches, "foreground") {
        println!(
            "{}",
            if daemon::Serve::new(crate::bottles::bindings).start()? {
                "Started folio serve"
            } else {
                "folio serve is already running"
            }
        );
        return Ok(0);
    }
    daemon::run(crate::bottles::bindings)
}

pub(super) fn serve_stop() -> Outcome {
    println!(
        "{}",
        if daemon::stop()? {
            "Stopped folio serve"
        } else {
            "folio serve isn't running"
        }
    );
    Ok(0)
}

pub(super) fn auth_list() -> Outcome {
    // Every credential is held by folio serve, so what's worth showing is where it's spent: the
    // hosts it's attached to, as the feature declares them (${option} is filled in from a bottle's
    // own options).
    let mut rows = vec![
        [
            "CREDENTIAL",
            "FEATURE",
            "STATUS",
            "ATTACHED TO",
            "DESCRIPTION",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for (declared, set) in auth::status()? {
        rows.push(vec![
            declared.credential.name.clone(),
            declared.feature.clone(),
            credential_status(&declared, set)?,
            declared.credential.inject.hosts.join(", "),
            declared.credential.description.clone(),
        ]);
    }
    table(&rows);
    Ok(0)
}

/// Whether the Keychain has it, and the file read before the Keychain, if the credential has one.
pub(super) fn credential_status(declared: &auth::Declared, set: bool) -> Result<String, String> {
    let status = if set { "set" } else { "not set" };
    let Some(properties) = &declared.credential.properties else {
        return Ok(status.into());
    };
    let mut path = properties.clone();
    for (name, value) in features::load(&declared.feature)?.values() {
        path = path.replace(&format!("${{{name}}}"), &value.text());
    }
    Ok(format!("{path}, else {status}"))
}

pub(super) fn auth_set(matches: &ArgMatches) -> Outcome {
    let name = arg(matches, "name").unwrap();
    let value = if std::io::stdin().is_terminal() {
        prompt::getpass(&format!("{name}: "))?
    } else {
        let mut value = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)
            .map_err(|e| e.to_string())?;
        value
    };
    auth::set_value(name, &value)?;
    println!("Stored {name}. Bottles with it get it when they next start.");
    Ok(0)
}

pub(super) fn auth_remove(matches: &ArgMatches) -> Outcome {
    let name = arg(matches, "name").unwrap();
    if auth::remove(name)? {
        println!("Removed {name}");
    } else {
        println!("{name} wasn't set");
    }
    Ok(0)
}

pub(super) fn repo_list() -> Outcome {
    let mut rows = vec![["REPO", "PATH", "THREADS"].map(String::from).to_vec()];
    for repo in repos::known() {
        let place = if repo.path == repos::owned_path(broker::NAME) {
            "(the broker)".to_string()
        } else if repo.path.is_dir() {
            repo.path.display().to_string()
        } else {
            format!("{} (gone)", repo.path.display())
        };
        rows.push(vec![
            repo.key(),
            place,
            threads::all(&repo)?.len().to_string(),
        ]);
    }
    table(&rows);
    Ok(0)
}
