//! Pausing a thread (its work and Claude's kept in a snapshot, its bottle deleted) and resuming it.

use super::*;

/// A bottle's state when its runner is off, as bottle says it: unreachable, or detached to be
/// turned off.
pub(super) const SNAPSHOT: [&str; 4] = [
    snapshots::WORKSPACE_ARCHIVE,
    snapshots::NOTES_ARCHIVE,
    snapshots::CLAUDE_ARCHIVE,
    snapshots::WORK,
];

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
/// The sessions are stopped first, and waited for, so Claude has written its conversation out.
/// Nothing is deleted until both archives are in the thread's slot and read back whole; if anything
/// fails before that, the bottle is left as it is.
pub fn pause(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<(), String> {
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
    stop_sessions(engine, thread)?;
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
        engine.run_to_file(&thread.bottle, &sh(snapshots::claude_out()), &claude)?;
        engine.run_to_file(
            &thread.bottle,
            &sh(work_script(&workdir(engine, thread)?)),
            &work,
        )?;
        snapshots::check(&claude)?;
        archived.push((snapshots::CLAUDE_ARCHIVE, Content::Move(claude)));
        archived.push((snapshots::WORK, Content::Move(work)));
        save(repo, thread, archived)
    })();
    let _ = fs::remove_dir_all(&staging);
    saved?;
    // Snapshotted: the pause is made, and what's left of it is done, interrupted or not.
    crate::interrupt::shielded(|| -> Result<(), String> {
        engine.delete(&thread.bottle)?;
        delete_sidecar(engine, thread)?;
        Ok(())
    })?;
    crate::log::log!("threads", "{}: paused", thread.name);
    Ok(())
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
    let (mut tips, mut dirty) = (BTreeMap::new(), false);
    for line in work.lines() {
        let (name, commit) = line.split_once('\t').unwrap_or((line, ""));
        if name == "(uncommitted changes)" {
            dirty = true;
        } else if !commit.is_empty() {
            tips.insert(name.to_string(), commit.to_string());
        }
    }
    let place = if state == State::Paused {
        "the snapshot"
    } else {
        "the bottle"
    };
    lost.extend(
        lanes::unsaved(&repo.path, &thread.name, &tips)?
            .into_iter()
            .map(|name| format!("{name}: in {place}, never pushed")),
    );
    if dirty {
        lost.push(format!("uncommitted changes in {place}"));
    }
    Ok(lost)
}
