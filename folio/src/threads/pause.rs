//! Pausing a thread (its work and Claude's kept in a snapshot, its bottle deleted) and resuming it.

use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::bottles::State;
use crate::engine::Engine;
use crate::repos::Repo;
use crate::serve::messages;
use crate::store::{self, Content};
use crate::{lanes, notes, snapshots};

use super::WORKSPACE;
use super::lifecycle::{bottle_mounts, make_bottle, workdir};
use super::sessions::{sh, stop_sessions};
use super::sidecar::delete_sidecar;
use super::state::state_of;
use super::{Thread, directory, mounts, save, snapshot};

/// Each mount's archive in a snapshot.
pub(super) fn archive_of(path: &str) -> Option<&'static str> {
    match path {
        WORKSPACE => Some(snapshots::WORKSPACE_ARCHIVE),
        notes::MOUNT => Some(snapshots::NOTES_ARCHIVE),
        _ => None,
    }
}

/// Lists a workspace's own branches and whether it has uncommitted changes, one per line,
/// tab-separated so a name is never split.
pub fn work_script(workspace: &str) -> String {
    let workspace = crate::shell::quote(workspace);
    format!(
        "cd {workspace} 2>/dev/null && git rev-parse --git-dir >/dev/null 2>&1 || exit 0
git for-each-ref --format='%(refname:short)%09%(objectname)' refs/heads
git symbolic-ref -q HEAD >/dev/null || printf '(detached HEAD)\\t%s\\n' \"$(git rev-parse HEAD)\"
[ -z \"$(git status --porcelain)\" ] || printf '(uncommitted changes)\\n'
"
    )
}

/// Snapshot the thread and delete its bottle: everything but its running processes is kept.
///
/// With `ask`, its agent is asked first to get ready, and the snapshot waits for it to say it is
/// (interrupt); unless it's done nothing since its last snapshot (activity::quiet_since), when it
/// was ready, and is still. The sessions are stopped then, and waited for, so Claude has written its
/// conversation out. Nothing is deleted until both archives are in the thread's slot and read back
/// whole; if anything fails before that, the bottle is left as it is.
pub fn pause(engine: &Engine, repo: &Repo, thread: &Thread, ask: bool) -> Result<(), String> {
    let state = state_of(engine, repo, thread)?;
    if state == State::Paused {
        return Err(format!(
            "thread {name} is already paused; `folio claude {name}` resumes it",
            name = thread.name
        ));
    }
    if state == State::Missing {
        return Err(format!(
            "thread {}'s bottle, {}, is gone; there's nothing to pause",
            thread.name, thread.bottle
        ));
    }
    if state != State::Running {
        engine.start(&thread.bottle)?;
    }
    let quiet = super::snapshot_taken(repo, thread).is_some_and(|t| {
        crate::activity::quiet_since(&thread.bottle, UNIX_EPOCH + Duration::from_secs_f64(t))
    });
    if ask && quiet {
        crate::log::say!(
            "threads",
            "folio: {}'s agent has done nothing since its last snapshot, so it isn't asked to get ready again",
            thread.name
        );
    } else if ask {
        interrupt(engine, repo, thread, Interrupt::Pause)?;
    }
    stop_sessions(engine, thread)?;
    take(engine, repo, thread)?;
    // Snapshotted: the pause is made, and what's left of it is done, interrupted or not.
    crate::interrupt::shielded(|| -> Result<(), String> {
        engine.delete(&thread.bottle)?;
        delete_sidecar(engine, thread)?;
        Ok(())
    })?;
    crate::log::log!("threads", "{}: paused", thread.name);
    Ok(())
}

/// Snapshot the running thread, its bottle kept, once its agent says it's ready, waiting `wait` for
/// it to (folio serve's, when the agent's been quiet a while: serve/idle.rs): whether it was
/// taken, or put off (the agent didn't say it was ready in time, or said something else). One with
/// no session to ask is taken. Claude goes on running: its archive is taken last, so whatever it
/// writes as it finishes its answer has the most time to be written.
pub fn snapshot_idle(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    wait: Duration,
) -> Result<bool, String> {
    if !interrupt(engine, repo, thread, Interrupt::Idle(wait))? {
        return Ok(false);
    }
    take(engine, repo, thread)?;
    crate::log::log!("threads", "{}: snapshotted, idle", thread.name);
    Ok(true)
}

/// Archive the thread's mounts, then what's unpushed, then Claude's, into a new slot of its, in
/// place of the snapshot before only once all are read back whole.
fn take(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<(), String> {
    let staging = store::staging(&directory(repo, &thread.name))?;
    let saved = (|| -> Result<(), String> {
        // None left from an older snapshot.
        let mut archived: Vec<(&str, Content)> = vec![
            (snapshots::WORKSPACE_ARCHIVE, Content::Leave),
            (snapshots::NOTES_ARCHIVE, Content::Leave),
        ];
        for path in bottle_mounts(engine, thread)? {
            let Some(name) = archive_of(&path) else {
                continue;
            };
            let archive = staging.join(name);
            engine.run_to_file(
                &thread.bottle,
                &sh(snapshots::workspace_out(&path)),
                &archive,
            )?;
            snapshots::check(&archive)?;
            for entry in archived.iter_mut().filter(|(n, _)| *n == name) {
                entry.1 = Content::Move(archive.clone());
            }
        }
        let claude = staging.join(snapshots::CLAUDE_ARCHIVE);
        let work = staging.join(snapshots::WORK);
        engine.run_to_file(
            &thread.bottle,
            &sh(work_script(&workdir(engine, thread)?)),
            &work,
        )?;
        engine.run_to_file(&thread.bottle, &sh(snapshots::claude_out()), &claude)?;
        snapshots::check(&claude)?;
        archived.push((snapshots::CLAUDE_ARCHIVE, Content::Move(claude)));
        archived.push((snapshots::WORK, Content::Move(work)));
        save(repo, thread, archived)
    })();
    let _ = fs::remove_dir_all(&staging);
    saved
}

/// Why the agent's asked to get ready: a pause the user asked for, which waits as long as it
/// takes; or folio serve's snapshot of an agent at rest, which it may put off by not answering
/// within the wait, or by answering anything else.
#[derive(Clone, Copy)]
enum Interrupt {
    Pause,
    Idle(Duration),
}

/// What the agent is told when it's asked to get ready for a snapshot (BOTTLE.md says what one is,
/// and what to do).
fn interrupt_text(repo: &Repo, thread: &Thread, why: Interrupt) -> String {
    let kept: Vec<String> = mounts(thread).iter().map(|p| format!("`{p}`")).collect();
    let last = match super::snapshot_size(repo, thread) {
        Some(size) => format!(
            "The last snapshot of this thread took {}.",
            crate::cli::output::human_size(size)
        ),
        None => "This thread has no snapshot yet.".into(),
    };
    let kept = format!(
        "It keeps {}, all but ignored files, and your conversation and memory. {last}",
        kept.join(" and ")
    );
    match why {
        Interrupt::Pause => format!(
            "Snapshot interrupt: the user is pausing this thread. Get ready for the snapshot \
             (BOTTLE.md, \"Snapshots and pauses\"), then reply `snapshot` to this message's from: the \
             pause waits for it. {kept}"
        ),
        Interrupt::Idle(_) => format!(
            "Automated snapshot interrupt. Ignore if actively executing background work. Reply \
             `snapshot` to this message's from after snapshot preparations (BOTTLE.md, \"Snapshots \
             and pauses\"). Avoid large untracked files in the workspace. {kept}"
        ),
    }
}

/// Ask the thread's agent to get ready for a snapshot, and wait for it to answer `snapshot`:
/// whether it did. Its answers come from folio serve, which forwards what the bottle says to the
/// snapshot address while this listens, and refuses it otherwise. One with no session to take the
/// event (Claude isn't running), or no channel to send it by, isn't asked: there's no one to get
/// ready, and it's as good as ready.
///
/// A pause waits as long as it takes: Ctrl-C stops waiting, and the pause; the agent's other
/// answers are shown, and waited past. An idle snapshot waits its while, and is put off by
/// anything else.
fn interrupt(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    why: Interrupt,
) -> Result<bool, String> {
    // Listening before it's asked: an answer can't come too soon.
    let mut listening = engine.listen(&thread.bottle)?;
    let from = messages::address(messages::SNAPSHOT, &thread.bottle)
        .map_err(|e| format!("couldn't ask: {e}"))?;
    let sent = engine.run(
        &thread.bottle,
        &[
            messages::BOTTLE_EVENT.to_string(),
            "send".into(),
            "-t".into(),
            "10".into(),
            "-f".into(),
            from,
        ],
        Some(&interrupt_text(repo, thread, why)),
        false,
    )?;
    if sent.code != 0 {
        let because = if sent.code == 75 {
            "no Claude session took the message".to_string()
        } else {
            format!("its channel didn't take the message: exit {}", sent.code)
        };
        match why {
            Interrupt::Pause => crate::log::say!(
                "threads",
                "folio: {}'s agent wasn't asked to get ready ({because}): snapshotting it as it is",
                thread.name
            ),
            Interrupt::Idle(_) => crate::log::log!(
                "threads",
                "{}: its agent wasn't asked to get ready ({because})",
                thread.name
            ),
        }
        return Ok(true);
    }
    if let Interrupt::Pause = why {
        crate::log::say!(
            "threads",
            "Asked {}'s agent to get ready for the snapshot; waiting for it to reply (Ctrl-C stops \
             the pause; `folio pause --force` doesn't ask)...",
            thread.name
        );
    }
    let until = match why {
        Interrupt::Idle(wait) => Some(Instant::now() + wait),
        Interrupt::Pause => None,
    };
    loop {
        crate::interrupt::check()?;
        if until.is_some_and(|until| Instant::now() >= until) {
            crate::log::log!("threads", "{}: its agent didn't answer", thread.name);
            return Ok(false);
        }
        let Some(answer) = listening.next(Duration::from_millis(200))? else {
            continue;
        };
        let ready = answer
            .trim()
            .trim_matches('`')
            .eq_ignore_ascii_case("snapshot");
        match why {
            Interrupt::Pause if ready => {
                crate::log::say!("threads", "{}'s agent is ready.", thread.name);
                return Ok(true);
            }
            Interrupt::Pause => crate::log::say!(
                "threads",
                "{}'s agent says: {}\n(still waiting for `snapshot`)",
                thread.name,
                answer.trim()
            ),
            Interrupt::Idle(_) => {
                if !ready {
                    crate::log::log!(
                        "threads",
                        "{}: its agent put the snapshot off: {}",
                        thread.name,
                        answer.trim()
                    );
                }
                return Ok(ready);
            }
        }
    }
}

/// A new bottle for a paused thread, with its snapshot restored; on any failure, still paused.
pub(super) fn resume(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<(), String> {
    let slot = snapshot(repo, thread).unwrap();
    if !thread.broker {
        lanes::register(&repo.path, &thread.name)?; // repairs it, if it was edited meanwhile
    }
    make_bottle(engine, repo, thread)?;
    let restored = (|| -> Result<(), String> {
        for path in mounts(thread) {
            let archive = slot.join(archive_of(path).unwrap());
            // A snapshot of a bottle that hadn't the mount (one from before notes) has nothing of
            // it to restore: it's checked out afresh.
            if archive.exists() {
                engine.run_from_file(
                    &thread.bottle,
                    &sh(snapshots::workspace_in(path)),
                    &archive,
                )?;
            }
        }
        // Where this bottle has each mount's objects and remotes: an older snapshot's checkout had
        // them elsewhere.
        let row = engine
            .status(std::slice::from_ref(&thread.bottle))?
            .remove(&thread.bottle)
            .unwrap();
        for mount in row.bottle.iter().flat_map(|b| &b.mounts) {
            let urls: BTreeMap<String, String> = mount
                .remotes
                .iter()
                .map(|r| (r.name.clone(), mount.url(&r.name)))
                .collect();
            let script = snapshots::repoint(&mount.path, &mount.objects, &urls);
            engine.run(&thread.bottle, &sh(script), None, true)?;
            // Then what the archive left to the index, from those objects.
            engine.run(
                &thread.bottle,
                &sh(snapshots::rebuild(&mount.path)),
                None,
                true,
            )?;
        }
        engine.run_from_file(
            &thread.bottle,
            &sh(snapshots::CLAUDE_IN),
            &slot.join(snapshots::CLAUDE_ARCHIVE),
        )?;
        Ok(())
    })();
    if let Err(e) = restored {
        crate::interrupt::shielded(|| {
            let _ = engine.delete(&thread.bottle);
            let _ = delete_sidecar(engine, thread);
        });
        return Err(e);
    }
    crate::log::log!("threads", "{}: resumed", thread.name);
    Ok(())
}

/// What deleting the thread would lose, for the user to read first.
///
/// Branches in the lane that aren't on the user's branches or origin's, and work that never reached
/// the lane: read from the bottle, which is started if it's stopped, or from a paused thread's
/// snapshot.
pub fn losses(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<Vec<String>, String> {
    if thread.broker {
        return Ok(Vec::new()); // no lane in a repo of yours, and its notes stay where they are
    }
    let mut lost: Vec<String> = lanes::unintegrated(&repo.path, &thread.name)?
        .into_iter()
        .map(|branch| format!("{branch}: pushed, but not on your branches or origin's"))
        .collect();
    let state = state_of(engine, repo, thread)?;
    if state == State::Missing {
        return Ok(lost);
    }
    let work = if state == State::Paused {
        // As the bottle saw it at pause: the snapshot itself is never opened here (see
        // snapshots.rs).
        let path = snapshot(repo, thread).unwrap().join(snapshots::WORK);
        fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        // A bottle that can't say what's in it (one broken from inside, say) is no reason to keep it:
        // that it couldn't is what's said to lose.
        let asked = if state == State::Running {
            Ok(())
        } else {
            engine.start(&thread.bottle)
        }
        .and_then(|()| {
            engine.run(
                &thread.bottle,
                &sh(work_script(&workdir(engine, thread)?)),
                None,
                true,
            )
        });
        match asked {
            Ok(out) => out.stdout,
            Err(e) => {
                crate::interrupt::check()?;
                lost.push(format!(
                    "whatever is in the bottle, which couldn't be asked what it has ({e})"
                ));
                return Ok(lost);
            }
        }
    };
    let place = if state == State::Paused {
        "the snapshot"
    } else {
        "the bottle"
    };
    lost.extend(unpushed(repo, &thread.name, &work, place)?);
    Ok(lost)
}

/// What `work` (work_script's, of a bottle of thread `name`'s) has that the lane doesn't: its
/// branches never pushed, and its uncommitted changes, said to be in `place`.
pub(super) fn unpushed(
    repo: &Repo,
    name: &str,
    work: &str,
    place: &str,
) -> Result<Vec<String>, String> {
    let (mut tips, mut dirty) = (BTreeMap::new(), false);
    for line in work.lines() {
        let (branch, commit) = line.split_once('\t').unwrap_or((line, ""));
        if branch == "(uncommitted changes)" {
            dirty = true;
        } else if !commit.is_empty() {
            tips.insert(branch.to_string(), commit.to_string());
        }
    }
    let mut lost: Vec<String> = lanes::unsaved(&repo.path, name, &tips)?
        .into_iter()
        .map(|branch| format!("{branch}: in {place}, never pushed"))
        .collect();
    if dirty {
        lost.push(format!("uncommitted changes in {place}"));
    }
    Ok(lost)
}
