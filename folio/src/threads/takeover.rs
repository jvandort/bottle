//! Taking a thread over: resuming it from its latest snapshot, here (or on another runner), while
//! its bottle is on a runner that's off, so a thread whose runner was turned off without a pause
//! goes on from where its snapshot has it. Whatever the bottle did since is thrown away: the user
//! agrees to that first (`folio claude` asks, when the runner is off).
//!
//! The bottle is left on its runner, folio's no longer (bottles::abandon): nothing starts it or
//! answers it, and folio serve answers a machine only by the runner it's bound on, so one of the
//! same name there reaches nothing of the thread's. The runner keeps a note of it (runner::leave,
//! which detaches it, so its coming back is an attach), and attaching it again asks to delete it,
//! once the thread has something newer than it: a bottle, or a snapshot taken since (clear_left).

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::bottles::{State, USER};
use crate::engine::Engine;
use crate::repos::{self, Repo};
use crate::{runner, runners, snapshots};

use super::pause::{resume, unpushed, work_script};
use super::state::states;
use super::{Settings, Thread, directory, load, save, snapshot, snapshot_taken};

/// What taking a thread over throws away, for the user to read before agreeing.
pub struct Takeover {
    /// The runner its bottle's on, and how that is (detached, or unreachable).
    pub runner: String,
    pub state: State,
    /// When its snapshot was taken.
    pub taken: f64,
    /// What the snapshot has that the lane doesn't.
    pub unpushed: Vec<String>,
}

/// What taking the thread over onto runner `to` would be; None if it can't be: its bottle isn't on
/// a runner that's off (or it's on `to`), or it has no snapshot to go on from.
pub fn takeover_plan(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    to: &str,
) -> Result<Option<Takeover>, String> {
    if thread.broker {
        return Ok(None);
    }
    let row = states(engine, repo, std::slice::from_ref(thread))?
        .remove(&thread.name)
        .unwrap();
    let state = row.status.state;
    let Some(place) = row.status.bottle.as_ref().map(|b| b.runner.clone()) else {
        return Ok(None);
    };
    if !state.runner_off() || place == to {
        return Ok(None);
    }
    let Some(taken) = snapshot_taken(repo, thread) else {
        return Ok(None);
    };
    let path = snapshot(repo, thread).unwrap().join(snapshots::WORK);
    let work = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(Takeover {
        runner: place,
        state,
        taken,
        unpushed: unpushed(repo, &thread.name, &work, "the snapshot")?,
    }))
}

/// Take the thread over onto runner `to`, as `plan` says, from its snapshot: its bottle left where
/// it is, its runner `to` from now on, and a new bottle made there with its snapshot restored. On
/// any failure, it's as it was: its bottle its own again, on its runner.
pub fn takeover(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    plan: &Takeover,
    to: &str,
) -> Result<Thread, String> {
    let old = engine.abandon(&thread.bottle)?;
    let about = json!({
        "repo": repo.path,
        "thread": thread.name,
        "workdir": old.workdir(),
        "at": now(),
    });
    let moved = Thread {
        settings: Settings {
            runner: Some(to.into()),
            ..thread.settings.clone()
        },
        ..thread.clone()
    };
    let done = runner::leave(&plan.runner, old.machine(), about)
        .map_err(|e| e.to_string())
        .and_then(|()| save(repo, &moved, Vec::new()))
        .and_then(|()| resume(engine, repo, &moved));
    if let Err(e) = done {
        crate::interrupt::shielded(|| {
            let _ = save(repo, thread, Vec::new());
            let _ = engine.readopt(&old, &directory(repo, &thread.name));
            let _ = runner::unleave(&plan.runner, old.machine());
        });
        return Err(e);
    }
    crate::log::log!(
        "threads",
        "{}: taken over from {}, onto {to}",
        thread.name,
        plan.runner
    );
    Ok(moved)
}

/// Delete the machines left on runner `name` when their threads were taken over, each that the user
/// agrees to, told first what it has that its thread's lane doesn't. One whose thread has nothing
/// newer (no bottle, nor a snapshot taken since) is kept, unasked: it's the thread's latest. One
/// whose thread isn't found (its repo moved, or it was deleted) is asked about as that. With no
/// terminal to ask on, each is kept, and the next attach asks again.
pub fn clear_left(name: &str) -> Result<(), String> {
    let on = runners::get(name)?;
    let mut failed = Vec::new();
    for (machine, about) in runner::left(name).map_err(|e| e.to_string())? {
        if on.state(&machine)?.is_none() {
            runner::unleave(name, &machine).map_err(|e| e.to_string())?;
            continue;
        }
        let thread_name = about["thread"].as_str().unwrap_or_default();
        let repo_path = about["repo"].as_str().unwrap_or_default();
        let at = about["at"].as_f64().unwrap_or_default();
        let found = repos::known()
            .into_iter()
            .find(|r| r.path.to_str() == Some(repo_path))
            .and_then(|r| load(&r, thread_name).ok().map(|t| (r, t)));
        let place = format!("{machine}, left on {name}");
        let lost = match &found {
            None => vec![format!(
                "what it has can't be told: there's no thread {thread_name} in a repo at {repo_path} \
                 (moved, or deleted)"
            )],
            Some((repo, thread)) => {
                let engine = Engine::default();
                let bottle =
                    engine.status(std::slice::from_ref(&thread.bottle))?[&thread.bottle].state;
                if bottle == State::Missing && !snapshot_taken(repo, thread).is_some_and(|t| t > at)
                {
                    println!(
                        "Kept {machine} on {name}: thread {thread_name}, taken over from it, has nothing newer"
                    );
                    continue;
                }
                let workdir = about["workdir"].as_str().unwrap_or("/workspace");
                let work = on.start(&machine).and_then(|()| {
                    on.exec(
                        &machine,
                        &["sh".into(), "-c".into(), work_script(workdir)],
                        Some(USER),
                        None,
                    )
                });
                match work {
                    Ok(work) => unpushed(repo, &thread.name, &work, &place)?,
                    Err(e) => vec![format!("whatever is in it, which couldn't be asked ({e})")],
                }
            }
        };
        println!(
            "{place} when thread {thread_name} was taken over, {}, has:",
            crate::cli::output::ago(at)
        );
        if lost.is_empty() {
            println!("  nothing the lane lacks");
        }
        for line in &lost {
            println!("  {line}");
        }
        if !runner::ask(&format!("Delete {machine}? [y/N] ")) {
            println!("Kept {machine} on {name}: `folio runner attach {name}` asks again");
            continue;
        }
        match on.delete(&machine) {
            Ok(()) => {
                runner::unleave(name, &machine).map_err(|e| e.to_string())?;
                println!("Deleted {machine}");
            }
            Err(e) => failed.push(format!("{machine}: {e}")),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "couldn't delete what's left on {name} ({}): `folio runner attach {name}` tries again",
            failed.join("; ")
        ))
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
