//! A thread's sessions (shpool's, in its bottle), and commands run in it.

use super::*;

/// What each session runs. Claude carries on with the thread's conversation if it has one, and
/// runs the claude feature's channel, bottle-event, if the bottle has it, so events and other
/// bottles' messages reach its session; both checked in the bottle, at the moment the session
/// starts. The channel isn't on Claude Code's allowlist, so it's loaded as a development channel,
/// which Claude Code asks to confirm each time it starts.
pub(super) fn session_script(session: &str) -> &'static str {
    match session {
        CLAUDE => concat!(
            "set --; ",
            // Claude keeps a directory's conversations under its path, /s and .s made -s.
            "if ls \"$HOME/.claude/projects/$(pwd | sed \"s|[/.]|-|g\")\"/*.jsonl >/dev/null 2>&1; ",
            "then set -- --continue; fi; ",
            "if command -v bottle-event >/dev/null; ",
            "then set -- \"$@\" --dangerously-load-development-channels server:events; fi; ",
            "exec claude \"$@\""
        ),
        _ => "exec bash -l",
    }
}

/// Attaches to the session named by $1, whose command is $2 (as shpool's --cmd takes it). shpool
/// loses a session's screen if a terminal attaches before it has a size, which an exec's can lack at
/// first.
///
/// An attach that fails at once is a session shpool can no longer show: its program may be running,
/// but the thread that passes its output on has died (as shpool-vterm 0.2.2 made it, on a zero-width
/// character), and every attach fails the same way until the session is killed. So it's started
/// again, once: Claude carries on with its conversation, which it keeps as it goes.
pub(super) fn attach_script() -> String {
    format!(
        "attach() {{
  for i in $(seq 20); do [ \"$(stty size)\" = \"0 0\" ] || {{ shpool attach -f \"$1\"; return; }}; sleep 0.1; done
  echo \"folio: the terminal has no size, so it wasn't attached\" >&2; exit 1
}}
started=$(date +%s)
attach \"$1\" && exit 0
status=$?
[ $(($(date +%s) - started)) -le 1 ] || exit $status
echo \"folio: shpool couldn't show the $1 session any more, so it's been started again\" >&2
shpool kill \"$1\"
env TERM={TERM} shpool attach --background --dir \"$3\" --cmd \"$2\" \"$1\" || exit $status
attach \"$1\"
"
    )
}

/// Replace folio with `argv` in the thread's bottle (or its sidecar), at its first mount, as genie:
/// its stdin, stdout and exit status this process's, and a terminal, if `tty`.
pub fn exec(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    argv: &[String],
    sidecar: bool,
    tty: bool,
) -> Result<std::convert::Infallible, String> {
    engine.attach(&bottle_for(engine, repo, thread, sidecar)?, argv, Some(tty))
}

/// Copy as `cp -r` would, between here and threads' bottles (or their sidecars): each side a thread
/// and a path in its bottle, a relative one from its first mount, or None and a path here. Where it
/// went.
pub fn copy(
    engine: &Engine,
    repo: &Repo,
    src: (Option<&Thread>, &str),
    dst: (Option<&Thread>, &str),
    sidecar: bool,
) -> Result<String, String> {
    let bottle = |thread: Option<&Thread>| {
        thread
            .map(|t| bottle_for(engine, repo, t, sidecar))
            .transpose()
    };
    let (src_bottle, dst_bottle) = (bottle(src.0)?, bottle(dst.0)?);
    engine.copy(
        (src_bottle.as_deref(), src.1),
        (dst_bottle.as_deref(), dst.1),
    )
}

/// Attach this terminal to one of the thread's sessions, bringing the thread to running first.
pub fn attach(
    engine: &Engine,
    repo: &Repo,
    thread: &Thread,
    session: &str,
) -> Result<std::convert::Infallible, String> {
    if !thread.broker {
        lanes::register(&repo.path, &thread.name)?; // brings its remote up to this folio's
    }
    ensure_running(engine, repo, thread)?;
    ensure_session(engine, thread, session)?;
    engine.touch(&thread.bottle)?;
    // The bottle has the work now, so a snapshot would only be out of date.
    if snapshot(repo, thread).is_some() {
        save(
            repo,
            thread,
            SNAPSHOT
                .iter()
                .map(|name| (*name, Content::Leave))
                .collect(),
        )?;
    }
    // -f detaches anything else on it: closing a terminal doesn't hang up its attach inside the VM.
    let argv: Vec<String> = vec![
        "sh".into(),
        "-c".into(),
        attach_script(),
        "attach".into(),
        session.into(),
        session_command(session),
        workdir(engine, thread)?,
    ];
    engine.attach(&thread.bottle, &argv, None)
}

pub(super) fn ensure_running(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<(), String> {
    let state = state_of(engine, repo, thread)?;
    match state {
        State::Paused => {
            crate::log::say!("threads", "Resuming thread {}...", thread.name);
            resume(engine, repo, thread)?;
        }
        State::Missing => {
            return Err(format!(
                "thread {name}'s bottle, {bottle}, is gone, and with it everything that wasn't pushed. What was \
                 pushed is in your repo: `git fetch bottle/{name}`. `folio delete {name}` removes what's left of \
                 the thread",
                name = thread.name,
                bottle = thread.bottle
            ));
        }
        State::Running => {}
        _ => engine.start(&thread.bottle)?,
    }
    start_sidecar(engine, thread)
}

pub(super) fn sh(script: impl Into<String>) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

/// Hang up the bottle's sessions, and wait (a while) for them to exit.
///
/// The hangup is folio's own: `shpool kill` follows its SIGHUP with a SIGKILL half a second later,
/// too soon for Claude to have written its conversation out.
pub(super) fn stop_sessions(engine: &Engine, thread: &Thread) -> Result<(), String> {
    let pids: Vec<String> = sessions(engine, thread)?
        .iter()
        .map(|s| s.shell_pid.to_string())
        .collect();
    if !pids.is_empty() {
        let mut kill = vec!["kill".to_string(), "-HUP".into()];
        kill.extend(pids.iter().cloned());
        engine.run(&thread.bottle, &kill, None, false)?;
        engine.run(
            &thread.bottle,
            &sh(format!(
                "for i in $(seq 50); do alive=; for p in {}; do kill -0 $p 2>/dev/null && alive=1; done; \
                 [ -z \"$alive\" ] && exit 0; sleep 0.2; done",
                pids.join(" ")
            )),
            None,
            true,
        )?;
    }
    Ok(())
}

/// A shpool session, as `shpool list --json` has it.
#[derive(Deserialize)]
pub(super) struct Session {
    name: String,
    shell_pid: i64,
}

/// The bottle's shpool sessions.
pub(super) fn sessions(engine: &Engine, thread: &Thread) -> Result<Vec<Session>, String> {
    let listed = engine.run(
        &thread.bottle,
        &["shpool".into(), "list".into(), "--json".into()],
        None,
        false,
    )?;
    if listed.code != 0 {
        return Ok(Vec::new());
    }
    #[derive(Deserialize)]
    struct Listed {
        sessions: Vec<Session>,
    }
    let said: Listed =
        serde_json::from_str(&listed.stdout).map_err(|e| format!("shpool list: {e}"))?;
    Ok(said.sessions)
}

pub(super) fn ensure_session(
    engine: &Engine,
    thread: &Thread,
    session: &str,
) -> Result<(), String> {
    if let Some(listed) = sessions(engine, thread)?
        .into_iter()
        .find(|s| s.name == session)
    {
        let alive = engine.run(
            &thread.bottle,
            &["kill".into(), "-0".into(), listed.shell_pid.to_string()],
            None,
            false,
        )?;
        if alive.code == 0 {
            return Ok(());
        }
        // Its program exited, but shpool keeps the session, and attaching to it would start a
        // plain shell in its place.
        engine.run(
            &thread.bottle,
            &["shpool".into(), "kill".into(), session.into()],
            None,
            false,
        )?;
    }
    let argv: Vec<String> = vec![
        "env".into(),
        format!("TERM={TERM}"),
        "shpool".into(),
        "attach".into(),
        "--background".into(),
        "--dir".into(),
        workdir(engine, thread)?,
        "--cmd".into(),
        session_command(session),
        session.into(),
    ];
    engine.run(&thread.bottle, &argv, None, true).map(|_| ())
}

/// What a session runs, as shpool's --cmd takes it.
pub fn session_command(session: &str) -> String {
    ["sh", "-c", session_script(session)]
        .iter()
        .map(|a| crate::shell::quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}
