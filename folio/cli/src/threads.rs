//! Threads: one line of work in one repo, and the bottle it's being worked in.
//!
//! A thread has a kebab-case name, unique within its repo; its bottle is named `<repo
//! key>-<thread>`, unique across repos. Its record is in its slot (see store.rs), and its state is
//! read live from bottle, never trusted from the record:
//!
//! ```text
//! running   the bottle is up: attach to it
//! stopped   the VM stopped, its disk intact (a crash, a restart, `bottle stop`): start it, and the
//!           work is all there
//! paused    no bottle, and a snapshot of it (see snapshots.rs): attaching resumes it in a new
//!           bottle, made from the repo's spec as it is now
//! missing   no bottle, and no snapshot: only what's in the lane is left. Folio never leaves a
//!           thread so (a snapshot exists exactly while there's no bottle); it's a bottle deleted
//!           from under it.
//! ```
//!
//! A thread has a snapshot only while it has no bottle: attaching to one drops it, since the bottle
//! is where the work is from then on.
//!
//! A thread can have a sidecar (`folio sidecar configure`): a bottle of a spec of its own, on a
//! runner of its own, `<thread's bottle>-sidecar`, with the thread's repos, fetch-only, which the
//! thread's bottle reaches as `ssh sidecar`. It's scratch: made just before the thread's bottle is,
//! started with it, and deleted with it, a pause included. So a sidecar given to a thread with a
//! bottle comes with the next one: pause and resume it.
//!
//! Inside the bottle, Claude and a shell each run in a shpool session of their own, started by one
//! exec and attached to by another, so closing a terminal only detaches it, and whatever the session
//! started keeps running. shpool passes the session's output straight through, so it's the
//! terminal's own scrollback that has it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::advice::{advice, durable, sidecar as sidecar_advice, sidecar_itself};
use crate::engine::{self, Mount};
use crate::repos::{Repo, check_repo_spec, default_spec};
use crate::store::{self, Content};
use crate::{broker, lanes, notes, snapshots};

const RECORD: &str = "thread.json";
pub const WORKSPACE: &str = "/workspace";
pub const CLAUDE: &str = "claude";
pub const SHELL: &str = "shell";
/// What a session's programs are told their terminal is. Not the one attached, whose TERM the
/// bottle may have no terminfo for, and which a session outlives.
const TERM: &str = "xterm-256color";
/// A bottle's state when its runner is off, as bottle says it: unreachable, or detached to be
/// turned off.
const RUNNER_OFF: [&str; 2] = ["unreachable", "detached"];
const SNAPSHOT: [&str; 4] = [
    snapshots::WORKSPACE_ARCHIVE,
    snapshots::NOTES_ARCHIVE,
    snapshots::CLAUDE_ARCHIVE,
    snapshots::WORK,
];

/// What each session runs. Claude carries on with the thread's conversation if it has one, and
/// runs the claude feature's channel, bottle-event, if the bottle has it, so events and other
/// bottles' messages reach its session; both checked in the bottle, at the moment the session
/// starts. The channel isn't on Claude Code's allowlist, so it's loaded as a development channel,
/// which Claude Code asks to confirm each time it starts.
fn session_script(session: &str) -> &'static str {
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
fn attach_script() -> String {
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

/// Each mount's archive in a snapshot.
fn archive_of(path: &str) -> Option<&'static str> {
    match path {
        WORKSPACE => Some(snapshots::WORKSPACE_ARCHIVE),
        notes::MOUNT => Some(snapshots::NOTES_ARCHIVE),
        _ => None,
    }
}

/// Lists a workspace's own branches and whether it has uncommitted changes, one per line,
/// tab-separated so a name is never split.
pub fn work_script(workspace: &str) -> String {
    let workspace = lanes::shell_quote(workspace);
    format!(
        "cd {workspace} 2>/dev/null && git rev-parse --git-dir >/dev/null 2>&1 || exit 0
git for-each-ref --format='%(refname:short)%09%(objectname)' refs/heads
git symbolic-ref -q HEAD >/dev/null || printf '(detached HEAD)\\t%s\\n' \"$(git rev-parse HEAD)\"
[ -z \"$(git status --porcelain)\" ] || printf '(uncommitted changes)\\n'
"
    )
}

/// A thread's sidecar: a runner of its own, and a spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sidecar {
    pub runner: String,
    pub spec: Value,
}

/// A thread's configuration: written only by what changes it (new, configure, rename, a runner
/// given), never by using it. What its bottle is, and when it was last used, is the bottle's
/// record's (bottles.rs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub name: String,
    pub bottle: String,
    pub created: f64,
    /// The branch it started from, if one was asked for; None: the repo's default.
    #[serde(default)]
    pub base: Option<String>,
    /// The broker (see broker.rs), whose bottle is made folio's way, not the repo's.
    #[serde(default)]
    pub broker: bool,
    /// The runner its bottles are made on, if it was given one; None: the repo's.
    #[serde(default)]
    pub runner: Option<String>,
    /// Its sidecar, if it has one.
    #[serde(default)]
    pub sidecar: Option<Sidecar>,
}

impl Thread {
    pub fn sidecar_bottle(&self) -> String {
        format!("{}-sidecar", self.bottle)
    }
}

/// A thread's name: lowercase letters and digits, in words joined by '-'.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|w| {
            !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

fn directory(repo: &Repo, name: &str) -> PathBuf {
    repo.directory().join(name)
}

pub fn exists(repo: &Repo, name: &str) -> bool {
    is_name(name) && store::current_slot(&directory(repo, name)).is_some()
}

fn read(slot: &Path) -> Result<Thread, String> {
    let path = slot.join(RECORD);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Keys a record from an older folio has (last_active, now its bottle's) are left out.
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn load(repo: &Repo, name: &str) -> Result<Thread, String> {
    let slot = if is_name(name) {
        store::current_slot(&directory(repo, name))
    } else {
        None
    };
    let Some(slot) = slot else {
        let known: Vec<String> = all(repo)?.into_iter().map(|t| t.name).collect();
        return Err(format!(
            "no thread named {} in {} (threads: {})",
            crate::repr(name),
            repo.path.display(),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ));
    };
    read(&slot)
}

/// The repo's threads, by name.
pub fn all(repo: &Repo) -> Result<Vec<Thread>, String> {
    let mut threads = Vec::new();
    for entry in fs::read_dir(repo.directory())
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if let Some(slot) = store::current_slot(&entry.path())
            && slot.join(RECORD).exists()
        {
            threads.push(read(&slot)?);
        }
    }
    threads.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(threads)
}

fn mtime(path: &Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// When the thread was last used, its state row (states) saying: its bottle's last use; a paused
/// thread's, when it was paused; else when it was made.
pub fn last_active(repo: &Repo, thread: &Thread, row: &Value) -> f64 {
    if let Some(when) = row
        .get("last_active")
        .and_then(Value::as_f64)
        .filter(|w| *w != 0.0)
    {
        return when;
    }
    match snapshot(repo, thread) {
        Some(slot) => mtime(&slot.join(snapshots::CLAUDE_ARCHIVE)),
        None => thread.created,
    }
}

/// `threads`, most recently active first.
pub fn recent(
    repo: &Repo,
    mut threads: Vec<Thread>,
    states: &BTreeMap<String, Value>,
) -> Vec<Thread> {
    let when = |t: &Thread| last_active(repo, t, &states[&t.name]);
    threads.sort_by(|a, b| when(b).total_cmp(&when(a)));
    threads
}

fn save(repo: &Repo, thread: &Thread, files: Vec<(&str, Content)>) -> Result<(), String> {
    let record = store::json(thread).into_bytes();
    let mut all = vec![(RECORD, Content::Bytes(record))];
    all.extend(files);
    store::write_slot(&directory(repo, &thread.name), all).map(|_| ())
}

/// The bytes a paused thread's snapshot takes up; None if it has none.
pub fn snapshot_size(repo: &Repo, thread: &Thread) -> Option<u64> {
    let slot = snapshot(repo, thread)?;
    Some(
        SNAPSHOT
            .iter()
            .filter_map(|name| fs::metadata(slot.join(name)).ok())
            .map(|m| m.len())
            .sum(),
    )
}

/// The slot holding the thread's snapshot, if it has one: every snapshot has Claude's archive.
fn snapshot(repo: &Repo, thread: &Thread) -> Option<PathBuf> {
    store::current_slot(&directory(repo, &thread.name))
        .filter(|slot| slot.join(snapshots::CLAUDE_ARCHIVE).exists())
}

/// Where the thread's notes are kept: in its directory, so a rename takes them along.
pub fn notes_dir(repo: &Repo, name: &str) -> PathBuf {
    directory(repo, name).join("notes")
}

/// The paths of the thread's bottle's mounts: its repo's checkout, and its notes; the broker, its
/// notes.
fn mounts(thread: &Thread) -> Vec<&'static str> {
    if thread.broker {
        vec![notes::MOUNT]
    } else {
        vec![WORKSPACE, notes::MOUNT]
    }
}

/// The paths of the mounts the thread's bottle has, as bottle has them -- a bottle made before this
/// folio may have fewer, or have them elsewhere -- else the thread's.
fn bottle_mounts(thread: &Thread) -> Result<Vec<String>, String> {
    let row = &engine::status(std::slice::from_ref(&thread.bottle))?[&thread.bottle];
    let paths: Vec<String> = row
        .get("mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m["path"].as_str().map(String::from))
        .collect();
    Ok(if paths.is_empty() {
        mounts(thread).into_iter().map(String::from).collect()
    } else {
        paths
    })
}

/// Where the thread's sessions start: its bottle's first mount.
fn workdir(thread: &Thread) -> Result<String, String> {
    Ok(bottle_mounts(thread)?.remove(0))
}

/// A new thread: its bottle (on `runner`, else the repo's) and its lane. Nothing runs in it until
/// something attaches.
pub fn create(
    repo: &Repo,
    name: &str,
    base: Option<&str>,
    is_broker: bool,
    runner: Option<&str>,
) -> Result<Thread, String> {
    if !is_name(name) {
        return Err(format!(
            "invalid thread name {}: use lowercase letters and digits, in words joined by '-'",
            crate::repr(name)
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
        runner: runner.map(String::from),
        sidecar: None,
    };
    make_bottle(repo, &thread)?;
    let registered = (|| {
        if !is_broker {
            lanes::register(&repo.path, name)?;
        }
        save(repo, &thread, Vec::new())
    })();
    if let Err(e) = registered {
        crate::interrupt::shielded(|| undo(repo, &thread));
        return Err(e);
    }
    Ok(thread)
}

/// Create the thread's bottle from the repo's spec as it is now, served the thread's lane.
fn make_bottle(repo: &Repo, thread: &Thread) -> Result<(), String> {
    if thread.broker {
        repo.remember()?;
        let notes_path = notes::ensure(&notes_dir(repo, &thread.name), "The broker's notes")?;
        let mounts_ = [Mount {
            path: notes::MOUNT.into(),
            repo: notes_path,
            checkout: None,
            remotes: notes::remotes(),
        }];
        return engine::new(
            &directory(repo, &thread.name),
            &thread.bottle,
            &engine::validate_spec(&broker::spec())?,
            &mounts_,
            &(broker::CONTEXT.to_string() + &durable(&mounts(thread))),
            None,
            None,
        );
    }
    // Checked again each time: a feature can have changed since the spec was saved. Checked as
    // saved: validating drops what bottle doesn't know.
    let spec = engine::validate_spec(&check_repo_spec(
        repo.spec()?.unwrap_or_else(default_spec),
        false,
    )?)?;
    repo.remember()?;
    if repo.spec()?.as_ref() != Some(&spec) {
        repo.save_spec(&spec)?;
    }
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
            if !delete_sidecar(thread)? {
                return Err(format!(
                    "thread {}'s old sidecar is left on a runner that's off, pushing where its bottle would: \
                     turn the runner on, then try again",
                    thread.name
                ));
            }
        }
        Some(sidecar) => match make_sidecar(repo, thread, &repos_) {
            // The thread is what's wanted: it goes on without, as when its sidecar doesn't start.
            Err(e) => crate::log::say!(
                "threads",
                "folio: thread {name} goes on without its sidecar ({e}); it comes with the thread's next bottle: \
                 pause and resume it (`folio pause {name}`, then `folio claude {name}`)",
                name = thread.name
            ),
            Ok(()) => {
                told = sidecar_advice(&sidecar.runner, &sidecar.spec);
                sidecar_bottle = Some(thread.sidecar_bottle());
            }
        },
    }
    // The thread's name goes with its bottle, so a bottle that lists the others sees which thread
    // each is.
    let mut labelled = spec.clone();
    labelled["labels"] = json!({"thread": thread.name});
    let made = engine::new(
        &directory(repo, &thread.name),
        &thread.bottle,
        &labelled,
        &repos_,
        &advice(&thread.name, &repo.name, &mounts(thread), &told),
        runner_of(repo, thread)?.as_deref(),
        sidecar_bottle.as_deref(),
    );
    if let Err(e) = made {
        if thread.sidecar.is_some()
            && let Err(cleanup) = crate::interrupt::shielded(|| delete_sidecar(thread))
        {
            // What went wrong is the bottle's: this is said beside it.
            crate::log::say!("threads", "folio: {cleanup}");
        }
        return Err(e);
    }
    Ok(())
}

/// Make the thread's sidecar afresh, from its spec as it is now: the thread's repos and remotes,
/// pushes included, which bottle allows as the thread's bottle may ssh to it.
fn make_sidecar(repo: &Repo, thread: &Thread, repos_: &[Mount]) -> Result<(), String> {
    let sidecar = thread.sidecar.as_ref().unwrap();
    // Scratch: whatever's left of one is no use.
    if !delete_sidecar(thread)? {
        return Err(format!(
            "its old one is on runner {}, which is off",
            sidecar.runner
        ));
    }
    let spec = check_repo_spec(engine::validate_spec(&sidecar.spec)?, true)?;
    engine::new(
        &directory(repo, &thread.name),
        &thread.sidecar_bottle(),
        &spec,
        repos_,
        &sidecar_itself(&thread.name, &repo.name),
        Some(&sidecar.runner),
        None,
    )
}

/// Delete the thread's sidecar, if it has one; false if its runner is off, which leaves it to be
/// deleted when the thread next gets one.
fn delete_sidecar(thread: &Thread) -> Result<bool, String> {
    if thread.broker {
        return Ok(true);
    }
    let name = thread.sidecar_bottle();
    let row = &engine::status(std::slice::from_ref(&name))?[&name];
    let state = row["state"].as_str().unwrap_or_default();
    if RUNNER_OFF.contains(&state) {
        crate::log::say!(
            "threads",
            "folio: thread {t}'s sidecar, {name}, is left on runner {r}, which is off: it's deleted, its record \
             with it, when the runner's on again and the thread next gets a bottle (or `folio delete {t}`)",
            t = thread.name,
            r = row["runner"].as_str().unwrap_or_default()
        );
        return Ok(false);
    }
    if state != "missing" {
        engine::delete(&name)?;
    }
    Ok(true)
}

/// Whether the thread's bottle was made with its sidecar (it went on without, else), its sidecar's
/// state, from one status of both.
fn sidecar_state(thread: &Thread) -> Result<(String, bool), String> {
    let names = [thread.bottle.clone(), thread.sidecar_bottle()];
    let rows = engine::status(&names)?;
    let state = rows[&names[1]]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let granted = rows[&names[0]]
        .get("ssh")
        .and_then(Value::as_object)
        .is_some_and(|ssh| ssh.values().any(|b| b.as_str() == Some(names[1].as_str())));
    Ok((state, granted))
}

/// Start the thread's sidecar with its bottle, if it has one that's stopped: bottle starts only the
/// one asked.
fn start_sidecar(thread: &Thread) -> Result<(), String> {
    if thread.sidecar.is_none() {
        return Ok(());
    }
    let (state, granted) = sidecar_state(thread)?;
    // One the thread's bottle wasn't made with (it went on without) is no use to it, if it's there
    // at all.
    if state == "missing" || !granted {
        crate::log::say!(
            "threads",
            "folio: thread {name}'s sidecar comes with its next bottle: pause and resume it \
             (`folio pause {name}`, then `folio claude {name}`)",
            name = thread.name
        );
    } else if state != "running"
        && let Err(e) = engine::start(&thread.sidecar_bottle())
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
pub fn sidecar_shell(repo: &Repo, thread: &Thread) -> Result<std::convert::Infallible, String> {
    engine::attach(
        &running_sidecar(repo, thread)?,
        &["bash".into(), "-l".into()],
        None,
    )
}

/// The thread's sidecar's bottle, while the thread runs, which is what it's for; refused, saying
/// why, else.
fn running_sidecar(repo: &Repo, thread: &Thread) -> Result<String, String> {
    if thread.sidecar.is_none() {
        return Err(format!(
            "thread {name} has no sidecar; give it one with `folio sidecar configure {name} --runner RUNNER ...`",
            name = thread.name
        ));
    }
    let state = state_of(repo, thread)?;
    if state != "running" {
        return Err(format!(
            "thread {name} is {state}: its sidecar is there while it runs (`folio claude {name}` or `folio shell {name}`)",
            name = thread.name
        ));
    }
    let (state, granted) = sidecar_state(thread)?;
    if state == "missing" || !granted {
        return Err(format!(
            "thread {name}'s sidecar comes with its next bottle: pause and resume it \
             (`folio pause {name}`, then `folio claude {name}`)",
            name = thread.name
        ));
    }
    Ok(thread.sidecar_bottle())
}

/// The bottle a command for the thread runs in: its own, the thread brought to running first
/// (resumed, if it's paused), as attach does; or, with `sidecar`, its sidecar, while the thread
/// runs.
fn bottle_for(repo: &Repo, thread: &Thread, sidecar: bool) -> Result<String, String> {
    if sidecar {
        return running_sidecar(repo, thread);
    }
    if !thread.broker {
        lanes::register(&repo.path, &thread.name)?;
    }
    ensure_running(repo, thread)?;
    Ok(thread.bottle.clone())
}

/// Replace folio with `argv` in the thread's bottle (or its sidecar), at its first mount, as genie:
/// its stdin, stdout and exit status this process's, and a terminal, if `tty`.
pub fn exec(
    repo: &Repo,
    thread: &Thread,
    argv: &[String],
    sidecar: bool,
    tty: bool,
) -> Result<std::convert::Infallible, String> {
    engine::attach(&bottle_for(repo, thread, sidecar)?, argv, Some(tty))
}

/// Copy as `cp -r` would, between here and threads' bottles (or their sidecars): each side a thread
/// and a path in its bottle, a relative one from its first mount, or None and a path here. Where it
/// went.
pub fn copy(
    repo: &Repo,
    src: (Option<&Thread>, &str),
    dst: (Option<&Thread>, &str),
    sidecar: bool,
) -> Result<String, String> {
    let bottle = |thread: Option<&Thread>| thread.map(|t| bottle_for(repo, t, sidecar)).transpose();
    let (src_bottle, dst_bottle) = (bottle(src.0)?, bottle(dst.0)?);
    engine::copy(
        (src_bottle.as_deref(), src.1),
        (dst_bottle.as_deref(), dst.1),
    )
}

/// The thread, with `sidecar` (None: none) from its next bottle on; its spec checked first.
pub fn set_sidecar(
    repo: &Repo,
    thread: &Thread,
    sidecar: Option<Sidecar>,
) -> Result<Thread, String> {
    if thread.broker {
        return Err("the broker has no sidecar".into());
    }
    let sidecar = match sidecar {
        Some(s) => Some(Sidecar {
            spec: check_repo_spec(engine::validate_spec(&s.spec)?, true)?,
            runner: s.runner,
        }),
        None => None,
    };
    let changed = Thread {
        sidecar,
        ..thread.clone()
    };
    save(repo, &changed, Vec::new())?;
    Ok(changed)
}

/// Where the thread's next bottle is made: its own runner, else the repo's; None for bottle's
/// default. The broker's is always here.
pub fn runner_of(repo: &Repo, thread: &Thread) -> Result<Option<String>, String> {
    if thread.broker {
        return Ok(None);
    }
    match &thread.runner {
        Some(runner) if !runner.is_empty() => Ok(Some(runner.clone())),
        _ => repo.runner(),
    }
}

/// The thread, its bottles made on `runner` from now on.
///
/// A paused thread resumes there. One with a bottle elsewhere is refused: its work is in that
/// bottle, and pausing it is what brings it back here, to go on.
pub fn place(repo: &Repo, thread: &Thread, runner: &str) -> Result<Thread, String> {
    if thread.broker {
        return Err("the broker runs on this machine".into());
    }
    let row = states(repo, std::slice::from_ref(thread))?
        .remove(&thread.name)
        .unwrap();
    let place = row.get("runner").and_then(Value::as_str).unwrap_or("local");
    let state = row["state"].as_str().unwrap_or_default();
    if !matches!(state, "paused" | "missing") && place != runner {
        return Err(format!(
            "thread {name}'s bottle is on {place}; pause it first (`folio pause {name}`), and it resumes on {runner}",
            name = thread.name
        ));
    }
    if thread.runner.as_deref() == Some(runner) {
        return Ok(thread.clone());
    }
    let placed = Thread {
        runner: Some(runner.into()),
        ..thread.clone()
    };
    save(repo, &placed, Vec::new())?;
    Ok(placed)
}

/// What create did, undone, as far as it got.
fn undo(repo: &Repo, thread: &Thread) {
    let _ = engine::delete(&thread.bottle);
    if !thread.broker {
        let _ = lanes::forget(&repo.path, &thread.name);
    }
    let _ = fs::remove_dir_all(directory(repo, &thread.name));
}

/// Each thread's bottle's status row, by thread name; a missing bottle with a snapshot is paused.
/// One with a sidecar has its sidecar's state too, as "sidecar".
pub fn states(repo: &Repo, threads: &[Thread]) -> Result<BTreeMap<String, Value>, String> {
    let mut names: Vec<String> = threads.iter().map(|t| t.bottle.clone()).collect();
    names.extend(
        threads
            .iter()
            .filter(|t| t.sidecar.is_some())
            .map(Thread::sidecar_bottle),
    );
    let rows = engine::status(&names)?;
    let mut result = BTreeMap::new();
    for t in threads {
        let mut row = rows
            .get(&t.bottle)
            .cloned()
            .unwrap_or_else(|| json!({"name": t.bottle, "state": "missing"}));
        if row["state"] == "missing" && snapshot(repo, t).is_some() {
            row["state"] = "paused".into();
        }
        if t.sidecar.is_some() {
            let sidecar = rows
                .get(&t.sidecar_bottle())
                .and_then(|r| r["state"].as_str())
                .unwrap_or("missing")
                .to_string();
            row["sidecar"] = sidecar.into();
        }
        result.insert(t.name.clone(), row);
    }
    Ok(result)
}

fn state_of(repo: &Repo, thread: &Thread) -> Result<String, String> {
    Ok(
        states(repo, std::slice::from_ref(thread))?[&thread.name]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

/// Attach this terminal to one of the thread's sessions, bringing the thread to running first.
pub fn attach(
    repo: &Repo,
    thread: &Thread,
    session: &str,
) -> Result<std::convert::Infallible, String> {
    if !thread.broker {
        lanes::register(&repo.path, &thread.name)?; // brings its remote up to this folio's
    }
    ensure_running(repo, thread)?;
    ensure_session(thread, session)?;
    engine::touch(&thread.bottle)?;
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
        workdir(thread)?,
    ];
    engine::attach(&thread.bottle, &argv, None)
}

fn ensure_running(repo: &Repo, thread: &Thread) -> Result<(), String> {
    let state = state_of(repo, thread)?;
    match state.as_str() {
        "paused" => {
            crate::log::say!("threads", "Resuming thread {}...", thread.name);
            resume(repo, thread)?;
        }
        "missing" => {
            return Err(format!(
                "thread {name}'s bottle, {bottle}, is gone, and with it everything that wasn't pushed. What was \
                 pushed is in your repo: `git fetch bottle/{name}`. `folio delete {name}` removes what's left of \
                 the thread",
                name = thread.name,
                bottle = thread.bottle
            ));
        }
        "running" => {}
        _ => engine::start(&thread.bottle)?,
    }
    start_sidecar(thread)
}

fn sh(script: impl Into<String>) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

/// Snapshot the thread and delete its bottle: everything but its running processes is kept.
///
/// The sessions are stopped first, and waited for, so Claude has written its conversation out.
/// Nothing is deleted until both archives are in the thread's slot and read back whole; if anything
/// fails before that, the bottle is left as it is.
pub fn pause(repo: &Repo, thread: &Thread) -> Result<(), String> {
    let state = state_of(repo, thread)?;
    if state == "paused" {
        return Err(format!(
            "thread {name} is already paused; `folio claude {name}` resumes it",
            name = thread.name
        ));
    }
    if state == "missing" {
        return Err(format!(
            "thread {}'s bottle, {}, is gone; there's nothing to pause",
            thread.name, thread.bottle
        ));
    }
    if state != "running" {
        engine::start(&thread.bottle)?;
    }
    stop_sessions(thread)?;
    let staging = store::staging(&directory(repo, &thread.name))?;
    let saved = (|| -> Result<(), String> {
        // None left from an older snapshot.
        let mut archived: Vec<(&str, Content)> = vec![
            (snapshots::WORKSPACE_ARCHIVE, Content::Leave),
            (snapshots::NOTES_ARCHIVE, Content::Leave),
        ];
        for path in bottle_mounts(thread)? {
            let Some(name) = archive_of(&path) else {
                continue;
            };
            let archive = staging.join(name);
            engine::run_to_file(
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
        engine::run_to_file(&thread.bottle, &sh(snapshots::claude_out()), &claude)?;
        engine::run_to_file(&thread.bottle, &sh(work_script(&workdir(thread)?)), &work)?;
        snapshots::check(&claude)?;
        archived.push((snapshots::CLAUDE_ARCHIVE, Content::Move(claude)));
        archived.push((snapshots::WORK, Content::Move(work)));
        save(repo, thread, archived)
    })();
    let _ = fs::remove_dir_all(&staging);
    saved?;
    // Snapshotted: the pause is made, and what's left of it is done, interrupted or not.
    crate::interrupt::shielded(|| -> Result<(), String> {
        engine::delete(&thread.bottle)?;
        delete_sidecar(thread)?;
        Ok(())
    })?;
    crate::log::log!("threads", "{}: paused", thread.name);
    Ok(())
}

/// Hang up the bottle's sessions, and wait (a while) for them to exit.
///
/// The hangup is folio's own: `shpool kill` follows its SIGHUP with a SIGKILL half a second later,
/// too soon for Claude to have written its conversation out.
fn stop_sessions(thread: &Thread) -> Result<(), String> {
    let pids: Vec<String> = sessions(thread)?
        .iter()
        .map(|s| s["shell_pid"].to_string())
        .collect();
    if !pids.is_empty() {
        let mut kill = vec!["kill".to_string(), "-HUP".into()];
        kill.extend(pids.iter().cloned());
        engine::run(&thread.bottle, &kill, None, false)?;
        engine::run(
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

/// A new bottle for a paused thread, with its snapshot restored; on any failure, still paused.
fn resume(repo: &Repo, thread: &Thread) -> Result<(), String> {
    let slot = snapshot(repo, thread).unwrap();
    if !thread.broker {
        lanes::register(&repo.path, &thread.name)?; // repairs it, if it was edited meanwhile
    }
    make_bottle(repo, thread)?;
    let restored = (|| -> Result<(), String> {
        for path in mounts(thread) {
            let mut archive = slot.join(archive_of(path).unwrap());
            // A broker from before notes had its vault at /workspace.
            if thread.broker && !archive.exists() {
                archive = slot.join(snapshots::WORKSPACE_ARCHIVE);
            }
            // A snapshot from before notes has none of them.
            if archive.exists() {
                engine::run_from_file(
                    &thread.bottle,
                    &sh(snapshots::workspace_in(path)),
                    &archive,
                )?;
            }
        }
        // Where this bottle has each mount's objects and remotes: an older snapshot's checkout had
        // them elsewhere.
        let row = engine::status(std::slice::from_ref(&thread.bottle))?
            .remove(&thread.bottle)
            .unwrap();
        for mount in row
            .get("mounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let urls: BTreeMap<String, String> = mount["urls"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect();
            let script = snapshots::repoint(
                mount["path"].as_str().unwrap_or_default(),
                mount["objects"].as_str().unwrap_or_default(),
                &urls,
            );
            engine::run(&thread.bottle, &sh(script), None, true)?;
        }
        engine::run_from_file(
            &thread.bottle,
            &sh(snapshots::CLAUDE_IN),
            &slot.join(snapshots::CLAUDE_ARCHIVE),
        )?;
        // Its conversation, too, was kept under the directory its vault was in.
        if thread.broker {
            engine::run(
                &thread.bottle,
                &sh(
                    "cd \"$HOME/.claude/projects\" 2>/dev/null && [ -d -workspace ] && [ ! -e -notes ] && mv -- -workspace -notes || true",
                ),
                None,
                true,
            )?;
        }
        Ok(())
    })();
    if let Err(e) = restored {
        crate::interrupt::shielded(|| {
            let _ = engine::delete(&thread.bottle);
            let _ = delete_sidecar(thread);
        });
        return Err(e);
    }
    crate::log::log!("threads", "{}: resumed", thread.name);
    Ok(())
}

/// The bottle's shpool sessions, as `shpool list --json` has them.
fn sessions(thread: &Thread) -> Result<Vec<Value>, String> {
    let listed = engine::run(
        &thread.bottle,
        &["shpool".into(), "list".into(), "--json".into()],
        None,
        false,
    )?;
    if listed.code != 0 {
        return Ok(Vec::new());
    }
    let said: Value =
        serde_json::from_str(&listed.stdout).map_err(|e| format!("shpool list: {e}"))?;
    Ok(said["sessions"].as_array().cloned().unwrap_or_default())
}

fn ensure_session(thread: &Thread, session: &str) -> Result<(), String> {
    if let Some(listed) = sessions(thread)?.into_iter().find(|s| s["name"] == session) {
        let alive = engine::run(
            &thread.bottle,
            &["kill".into(), "-0".into(), listed["shell_pid"].to_string()],
            None,
            false,
        )?;
        if alive.code == 0 {
            return Ok(());
        }
        // Its program exited, but shpool keeps the session, and attaching to it would start a
        // plain shell in its place.
        engine::run(
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
        workdir(thread)?,
        "--cmd".into(),
        session_command(session),
        session.into(),
    ];
    engine::run(&thread.bottle, &argv, None, true).map(|_| ())
}

/// What a session runs, as shpool's --cmd takes it.
pub fn session_command(session: &str) -> String {
    ["sh", "-c", session_script(session)]
        .iter()
        .map(|a| lanes::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What deleting the thread would lose, for the user to read first.
///
/// Branches in the lane that aren't on the user's branches or origin's, and work that never reached
/// the lane: read from the bottle, which is started if it's stopped, or from a paused thread's
/// snapshot.
pub fn losses(repo: &Repo, thread: &Thread) -> Result<Vec<String>, String> {
    if thread.broker {
        return Ok(Vec::new()); // no lane in a repo of yours, and its notes stay where they are
    }
    let mut lost: Vec<String> = lanes::unintegrated(&repo.path, &thread.name)?
        .into_iter()
        .map(|branch| format!("{branch}: pushed, but not on your branches or origin's"))
        .collect();
    let state = state_of(repo, thread)?;
    if state == "missing" {
        return Ok(lost);
    }
    let work = if state == "paused" {
        // As the bottle saw it at pause: the snapshot itself is never opened here (see
        // snapshots.rs).
        let path = snapshot(repo, thread).unwrap().join(snapshots::WORK);
        fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        // A bottle that can't say what's in it (one broken from inside, say) is no reason to keep it:
        // that it couldn't is what's said to lose.
        let asked = if state == "running" {
            Ok(())
        } else {
            engine::start(&thread.bottle)
        }
        .and_then(|()| {
            engine::run(
                &thread.bottle,
                &sh(work_script(&workdir(thread)?)),
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
    let place = if state == "paused" {
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

/// Call the thread `new`: its record, its lane and its remote, and the bottle it gets next.
///
/// Only without a bottle: a bottle's name is its thread's, and can't change, so a running or
/// stopped thread is paused first. Its branches keep their names; the addresses other bottles had
/// for it stop working, as its bottle's name changes.
pub fn rename(repo: &Repo, thread: &Thread, new: &str) -> Result<Thread, String> {
    if !is_name(new) {
        return Err(format!(
            "invalid thread name {}: use lowercase letters and digits, in words joined by '-'",
            crate::repr(new)
        ));
    }
    if store::current_slot(&directory(repo, new)).is_some() {
        return Err(format!("thread {new} already exists"));
    }
    if thread.broker {
        return Err("the broker is called the broker".into());
    }
    let state = state_of(repo, thread)?;
    if !matches!(state.as_str(), "paused" | "missing") {
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
pub fn delete(repo: &Repo, thread: &Thread) -> Result<(), String> {
    if !matches!(state_of(repo, thread)?.as_str(), "missing" | "paused") {
        engine::delete(&thread.bottle)?;
    }
    if !thread.broker {
        delete_sidecar(thread)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_kebab_case() {
        assert!(is_name("fix-it") && is_name("a1") && is_name("x-2-y"));
        assert!(
            !is_name("")
                && !is_name("Fix")
                && !is_name("-x")
                && !is_name("x-")
                && !is_name("a--b")
                && !is_name("a_b")
        );
    }

    #[test]
    fn a_session_runs_as_shpool_takes_it() {
        assert_eq!(session_command(SHELL), "sh -c 'exec bash -l'");
        assert!(session_command(CLAUDE).starts_with("sh -c 'set --; if ls"));
    }

    #[test]
    fn a_record_reads_back_and_one_from_an_older_folio_reads() {
        let thread = Thread {
            name: "t".into(),
            bottle: "r-t".into(),
            created: 1.5,
            base: None,
            broker: false,
            runner: None,
            sidecar: Some(Sidecar {
                runner: "box".into(),
                spec: json!({"memory": "2G", "features": []}),
            }),
        };
        assert_eq!(
            serde_json::from_str::<Thread>(&store::json(&thread)).unwrap(),
            thread
        );
        // Keys it no longer has (last_active, now its bottle's) are left out.
        let read: Thread =
            serde_json::from_str(r#"{"name": "t", "bottle": "b", "created": 1, "last_active": 2}"#)
                .unwrap();
        assert_eq!((read.broker, read.sidecar), (false, None));
    }
}
