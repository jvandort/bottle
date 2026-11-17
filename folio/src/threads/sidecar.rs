//! A thread's sidecar: a bottle of its own, which the thread's may ssh to.

use super::*;

/// Make the thread's sidecar afresh, from its spec as it is now: the thread's repos and remotes,
/// pushes included, which bottle allows as the thread's bottle may ssh to it.
pub(super) fn make_sidecar(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    repos_: &[Mount],
) -> Result<(), String> {
    let sidecar = thread.sidecar.as_ref().unwrap();
    // Scratch: whatever's left of one is no use.
    let runner = sidecar.runner.as_deref();
    if !delete_sidecar(engine, thread)? {
        return Err(format!(
            "its old one is on runner {}, which is off",
            runner.unwrap_or_default()
        ));
    }
    let spec = check_repo_spec(engine::validate_spec(&sidecar.over(&sidecar_base()))?, true)?;
    engine.create(&Order {
        owner: &directory(repo, &thread.name),
        name: &thread.sidecar_bottle(),
        spec: &spec,
        mounts: repos_,
        context: &sidecar_itself(&thread.name, &repo.name),
        runner,
        sidecar: None,
    })
}

/// Delete the thread's sidecar, if it has one; false if its runner is off, which leaves it to be
/// deleted when the thread next gets one.
pub(super) fn delete_sidecar(engine: &Engine, thread: &Thread) -> Result<bool, String> {
    if thread.broker {
        return Ok(true);
    }
    let name = thread.sidecar_bottle();
    let row = &engine.status(std::slice::from_ref(&name))?[&name];
    if row.state.runner_off() {
        crate::log::say!(
            "threads",
            "folio: thread {t}'s sidecar, {name}, is left on runner {r}, which is off: it's deleted, its record \
             with it, when the runner's on again and the thread next gets a bottle (or `folio delete {t}`)",
            t = thread.name,
            r = row.bottle.as_ref().map_or("", |b| b.runner.as_str())
        );
        return Ok(false);
    }
    if row.state != State::Missing {
        engine.delete(&name)?;
    }
    Ok(true)
}

/// Whether the thread's bottle was made with its sidecar (it went on without, else), its sidecar's
/// state, from one status of both.
pub(super) fn sidecar_state(engine: &Engine, thread: &Thread) -> Result<(State, bool), String> {
    let names = [thread.bottle.clone(), thread.sidecar_bottle()];
    let rows = engine.status(&names)?;
    let state = rows[&names[1]].state;
    let granted = rows[&names[0]]
        .bottle
        .as_ref()
        .is_some_and(|b| b.ssh.iter().any(|g| g.bottle == names[1]));
    Ok((state, granted))
}

/// Start the thread's sidecar with its bottle, if it has one that's stopped: bottle starts only the
/// one asked.
pub(super) fn start_sidecar(engine: &Engine, thread: &Thread) -> Result<(), String> {
    if thread.sidecar.is_none() {
        return Ok(());
    }
    let (state, granted) = sidecar_state(engine, thread)?;
    // One the thread's bottle wasn't made with (it went on without) is no use to it, if it's there
    // at all.
    if state == State::Missing || !granted {
        crate::log::say!(
            "threads",
            "folio: thread {name}'s sidecar comes with its next bottle: pause and resume it \
             (`folio pause {name}`, then `folio claude {name}`)",
            name = thread.name
        );
    } else if state != State::Running
        && let Err(e) = engine.start(&thread.sidecar_bottle())
    {
        crate::log::say!(
            "threads",
            "folio: thread {}'s sidecar didn't start ({e}); the thread goes on without it",
            thread.name
        );
    }
    Ok(())
}

/// Replace folio with a login shell in the thread's sidecar, at its first mount: only while the
/// thread is running, which is what the sidecar is for. Nothing keeps it: closing the terminal ends
/// it.
pub fn sidecar_shell(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
) -> Result<std::convert::Infallible, String> {
    engine.attach(
        &running_sidecar(engine, repo, thread)?,
        &["bash".into(), "-l".into()],
        None,
    )
}

/// The thread's sidecar's bottle, while the thread runs, which is what it's for; refused, saying
/// why, else.
pub(super) fn running_sidecar(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
) -> Result<String, String> {
    if thread.sidecar.is_none() {
        return Err(format!(
            "thread {name} has no sidecar; give it one with `folio sidecar configure {name} --runner RUNNER ...`",
            name = thread.name
        ));
    }
    let state = state_of(engine, repo, thread)?;
    if state != State::Running {
        return Err(format!(
            "thread {name} is {state}: its sidecar is there while it runs (`folio claude {name}` or `folio shell {name}`)",
            name = thread.name
        ));
    }
    let (state, granted) = sidecar_state(engine, thread)?;
    if state == State::Missing || !granted {
        return Err(format!(
            "thread {name}'s sidecar comes with its next bottle: pause and resume it \
             (`folio pause {name}`, then `folio claude {name}`)",
            name = thread.name
        ));
    }
    Ok(thread.sidecar_bottle())
}

/// The thread, with `sidecar` (None: none) from its next bottle on; its spec checked first.
pub fn set_sidecar(
    repo: &Repo,
    thread: &Thread,
    sidecar: Option<Settings>,
) -> Result<Thread, String> {
    if thread.broker {
        return Err("the broker has no sidecar".into());
    }
    if let Some(sidecar) = &sidecar {
        check_repo_spec(engine::validate_spec(&sidecar.over(&sidecar_base()))?, true)?;
    }
    let changed = Thread {
        sidecar,
        ..thread.clone()
    };
    save(repo, &changed, Vec::new())?;
    Ok(changed)
}
