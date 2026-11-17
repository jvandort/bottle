//! A thread's bottle made, and the thread changed, renamed and deleted.

use super::*;

/// The paths of the mounts the thread's bottle has, as bottle has them -- a bottle made before this
/// folio may have fewer, or have them elsewhere -- else the thread's.
pub(super) fn bottle_mounts(engine: &Engine, thread: &Thread) -> Result<Vec<String>, String> {
    let row = &engine.status(std::slice::from_ref(&thread.bottle))?[&thread.bottle];
    let paths: Vec<String> = row
        .bottle
        .iter()
        .flat_map(|b| &b.mounts)
        .map(|m| m.path.clone())
        .collect();
    Ok(if paths.is_empty() {
        mounts(thread).into_iter().map(String::from).collect()
    } else {
        paths
    })
}

/// Where the thread's sessions start: its bottle's first mount.
pub(super) fn workdir(engine: &Engine, thread: &Thread) -> Result<String, String> {
    Ok(bottle_mounts(engine, thread)?.remove(0))
}

/// A new thread: its bottle (on `runner`, else the repo's) and its lane. Nothing runs in it until
/// something attaches.
pub fn create(
    engine: &Engine,
    repo: &Repo,
    name: &str,
    base: Option<&str>,
    is_broker: bool,
    runner: Option<&str>,
) -> Result<Thread, String> {
    if !is_name(name) {
        return Err(format!(
            "invalid thread name {}: use lowercase letters and digits, in words joined by '-'",
            crate::quoted(name)
        ));
    }
    if store::current_slot(&directory(repo, name)).is_some() {
        return Err(format!(
            "thread {name} already exists; attach with `folio claude {name}`"
        ));
    }
    let thread = Thread {
        name: name.into(),
        bottle: if is_broker {
            broker::NAME.into()
        } else {
            format!("{}-{name}", repo.key())
        },
        created: now(),
        base: base.map(String::from),
        broker: is_broker,
        settings: Settings {
            runner: runner.map(String::from),
            ..Settings::default()
        },
        sidecar: None,
    };
    make_bottle(engine, repo, &thread)?;
    let registered = (|| {
        if !is_broker {
            lanes::register(&repo.path, name)?;
        }
        save(repo, &thread, Vec::new())
    })();
    if let Err(e) = registered {
        crate::interrupt::shielded(|| undo(engine, repo, &thread));
        return Err(e);
    }
    Ok(thread)
}

/// Create the thread's bottle from the repo's spec as it is now, served the thread's lane.
pub(super) fn make_bottle(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<(), String> {
    if thread.broker {
        repo.remember()?;
        let notes_path = notes::ensure(&notes_dir(repo, &thread.name), "The broker's notes")?;
        let mounts_ = [Mount {
            path: notes::MOUNT.into(),
            repo: notes_path,
            checkout: None,
            remotes: notes::remotes(),
        }];
        return engine.create(&Order {
            owner: &directory(repo, &thread.name),
            name: &thread.bottle,
            spec: &engine::validate_spec(&broker::spec())?,
            mounts: &mounts_,
            context: &(broker::CONTEXT.to_string() + &durable(&mounts(thread))),
            runner: None,
            sidecar: None,
        });
    }
    // Checked again each time: a feature can have changed since the spec was saved. The repo's is
    // saved as checked: validating drops what bottle doesn't know.
    let repos_spec = engine::validate_spec(&check_repo_spec(
        repo.spec()?.unwrap_or_else(default_spec),
        false,
    )?)?;
    repo.remember()?;
    if repo.spec()?.as_ref() != Some(&repos_spec) {
        repo.save_spec(&repos_spec)?;
    }
    let spec = if thread.settings.changes_spec() {
        engine::validate_spec(&check_repo_spec(thread.settings.over(&repos_spec), false)?)?
    } else {
        repos_spec
    };
    let notes_path = notes::ensure(
        &notes_dir(repo, &thread.name),
        &format!("Notes of thread {}", thread.name),
    )?;
    let repos_ = vec![
        Mount {
            path: WORKSPACE.into(),
            repo: repo.path.clone(),
            checkout: thread.base.clone(),
            remotes: lanes::bottle_remotes(&thread.name),
        },
        Mount {
            path: notes::MOUNT.into(),
            repo: notes_path,
            checkout: None,
            remotes: notes::remotes(),
        },
    ];
    let mut told = String::new();
    let mut sidecar_bottle = None;
    match &thread.sidecar {
        None => {
            // One left from before it was taken away: it pushes into the thread's lane, as this
            // bottle will.
            if !delete_sidecar(engine, thread)? {
                return Err(format!(
                    "thread {}'s old sidecar is left on a runner that's off, pushing where its bottle would: \
                     turn the runner on, then try again",
                    thread.name
                ));
            }
        }
        Some(sidecar) => match make_sidecar(engine, repo, thread, &repos_) {
            // The thread is what's wanted: it goes on without, as when its sidecar doesn't start.
            Err(e) => crate::log::say!(
                "threads",
                "folio: thread {name} goes on without its sidecar ({e}); it comes with the thread's next bottle: \
                 pause and resume it (`folio pause {name}`, then `folio claude {name}`)",
                name = thread.name
            ),
            Ok(()) => {
                told = sidecar_advice(
                    sidecar.runner.as_deref().unwrap_or_default(),
                    &sidecar.over(&sidecar_base()),
                );
                sidecar_bottle = Some(thread.sidecar_bottle());
            }
        },
    }
    // The thread's name goes with its bottle, so a bottle that lists the others sees which thread
    // each is.
    let mut labelled = spec.clone();
    labelled.labels = BTreeMap::from([("thread".into(), thread.name.clone())]);
    let runner = runner_of(repo, thread)?;
    let made = engine
        .create(&Order {
            owner: &directory(repo, &thread.name),
            name: &thread.bottle,
            spec: &labelled,
            mounts: &repos_,
            context: &advice(&thread.name, &repo.name, &mounts(thread), &told),
            runner: runner.as_deref(),
            sidecar: sidecar_bottle.as_deref(),
        })
        .map_err(|e| elsewhere(thread, runner.as_deref(), e));
    if let Err(e) = made {
        if thread.sidecar.is_some()
            && let Err(cleanup) = crate::interrupt::shielded(|| delete_sidecar(engine, thread))
        {
            // What went wrong is the bottle's: this is said beside it.
            crate::log::say!("threads", "folio: {cleanup}");
        }
        return Err(e);
    }
    Ok(())
}

/// `e`, from making the thread's bottle on `runner`, with where else it can go, if the runner's
/// detached: one turned off for good may be the reason.
pub(super) fn elsewhere(thread: &Thread, runner: Option<&str>, e: String) -> String {
    let Some(runner) = runner else {
        return e;
    };
    if !crate::runner::detached().is_ok_and(|d| d.get(runner).copied().unwrap_or(false)) {
        return e;
    }
    format!(
        "{e}; or make its bottle on this machine instead: `folio claude {name} --runner local`",
        name = thread.name
    )
}

/// The bottle a command for the thread runs in: its own, the thread brought to running first
/// (resumed, if it's paused), as attach does; or, with `sidecar`, its sidecar, while the thread
/// runs.
pub(super) fn bottle_for(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    sidecar: bool,
) -> Result<String, String> {
    if sidecar {
        return running_sidecar(engine, repo, thread);
    }
    if !thread.broker {
        lanes::register(&repo.path, &thread.name)?;
    }
    ensure_running(engine, repo, thread)?;
    Ok(thread.bottle.clone())
}

/// The thread, with `settings` from its next bottle on; its spec, over the repo's, checked first.
///
/// A paused thread resumes on the runner they come to. One with a bottle elsewhere is refused: its
/// work is in that bottle, and pausing it is what brings it back here, to go on.
pub fn set_settings(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    settings: Settings,
) -> Result<Thread, String> {
    if thread.broker {
        return Err(
            "the broker's bottle is folio's: it runs on this machine, from folio's spec".into(),
        );
    }
    if settings.changes_spec() {
        check_repo_spec(engine::validate_spec(&settings.spec(repo)?)?, false)?;
    }
    let runner = settings.runner(repo)?;
    let runner = runner.as_deref().unwrap_or(runners::LOCAL);
    let row = states(engine, repo, std::slice::from_ref(thread))?
        .remove(&thread.name)
        .unwrap();
    let place = row
        .status
        .bottle
        .as_ref()
        .map_or(runners::LOCAL, |b| b.runner.as_str());
    let state = row.status.state;
    if !matches!(state, State::Paused | State::Missing) && place != runner {
        return Err(format!(
            "thread {name}'s bottle is on {place}; pause it first (`folio pause {name}`), and it resumes on {runner}",
            name = thread.name
        ));
    }
    if thread.settings == settings {
        return Ok(thread.clone());
    }
    let changed = Thread {
        settings,
        ..thread.clone()
    };
    save(repo, &changed, Vec::new())?;
    Ok(changed)
}

/// What create did, undone, as far as it got.
pub(super) fn undo(engine: &Engine, repo: &Repo, thread: &Thread) {
    let _ = engine.delete(&thread.bottle);
    if !thread.broker {
        let _ = lanes::forget(&repo.path, &thread.name);
    }
    let _ = fs::remove_dir_all(directory(repo, &thread.name));
}

/// Call the thread `new`: its record, its lane and its remote, and the bottle it gets next.
///
/// Only without a bottle: a bottle's name is its thread's, and can't change, so a running or
/// stopped thread is paused first. Its branches keep their names; the addresses other bottles had
/// for it stop working, as its bottle's name changes.
pub fn rename(engine: &Engine, repo: &Repo, thread: &Thread, new: &str) -> Result<Thread, String> {
    if !is_name(new) {
        return Err(format!(
            "invalid thread name {}: use lowercase letters and digits, in words joined by '-'",
            crate::quoted(new)
        ));
    }
    if store::current_slot(&directory(repo, new)).is_some() {
        return Err(format!("thread {new} already exists"));
    }
    if thread.broker {
        return Err("the broker is called the broker".into());
    }
    let state = state_of(engine, repo, thread)?;
    if !matches!(state, State::Paused | State::Missing) {
        return Err(format!(
            "thread {name} has a bottle, whose name can't change: pause it first (`folio pause {name}`), then rename it",
            name = thread.name
        ));
    }
    lanes::rename(&repo.path, &thread.name, new)?;
    fs::rename(directory(repo, &thread.name), directory(repo, new)).map_err(|e| e.to_string())?;
    let renamed = Thread {
        name: new.into(),
        bottle: format!("{}-{new}", repo.key()),
        ..thread.clone()
    };
    save(repo, &renamed, Vec::new())?;
    crate::log::log!("threads", "{}: renamed {new}", thread.name);
    Ok(renamed)
}

/// Delete the thread: its bottle, its lane and remote, and its record and snapshot. Not its notes:
/// they stay where they are, and a new thread of the same name finds them.
pub fn delete(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<(), String> {
    if !matches!(
        state_of(engine, repo, thread)?,
        State::Missing | State::Paused
    ) {
        engine.delete(&thread.bottle)?;
    }
    if !thread.broker {
        delete_sidecar(engine, thread)?;
        lanes::forget(&repo.path, &thread.name)?;
    }
    let dir = directory(repo, &thread.name);
    let kept = notes_dir(repo, &thread.name);
    for entry in fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        if path == kept {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let removed = if is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(|e| format!("{}: {e}", path.display()))?;
    }
    if !kept.exists() {
        fs::remove_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    crate::log::log!("threads", "{}: deleted", thread.name);
    Ok(())
}
