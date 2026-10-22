"""Threads: one line of work in one repo, and the bottle it's being worked in.

A thread has a kebab-case name, unique within its repo; its bottle is named
`<repo key>-<thread>`, unique across repos. Its record is in its slot (see
store.py), and its state is read live from bottle, never trusted from the
record:

    running   the bottle is up: attach to it
    stopped   the VM stopped, its disk intact (a crash, a restart, `bottle
              stop`): start it, and the work is all there
    paused    no bottle, and a snapshot of it (see snapshots.py): attaching
              resumes it in a new bottle, made from the repo's spec as it is now
    missing   no bottle, and no snapshot: only what's in the lane is left.
              Folio never leaves a thread so (a snapshot exists exactly while
              there's no bottle); it's a bottle deleted from under it.

A thread has a snapshot only while it has no bottle: attaching to one drops
it, since the bottle is where the work is from then on.

Inside the bottle, Claude and a shell each run in a shpool session of their
own, started by one exec and attached to by another, so closing a terminal only
detaches it, and whatever the session started keeps running. shpool passes the
session's output straight through, so it's the terminal's own scrollback that
has it.
"""

import json
import re
import shlex
import shutil
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import NoReturn

from . import engine, lanes, snapshots, store
from .advice import advice
from .errors import FolioError
from .repos import DEFAULT_SPEC, Repo

NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
RECORD = "thread.json"
WORKSPACE = "/workspace"
CLAUDE, SHELL = "claude", "shell"
# What each session runs. Claude carries on with the thread's conversation if
# it has one; checked in the bottle, at the moment the session starts.
SESSIONS = {
    CLAUDE: 'if ls "$HOME"/.claude/projects/-workspace/*.jsonl >/dev/null 2>&1; '
            'then exec claude --continue; else exec claude; fi',
    SHELL: "exec bash -l",
}
# What a session's programs are told their terminal is. Not the one attached,
# whose TERM the bottle may have no terminfo for, and which a session outlives.
TERM = "xterm-256color"
# Attaches to the session named by $1. shpool loses a session's screen if a
# terminal attaches before it has a size, which an exec's can lack at first.
ATTACH = """\
for i in $(seq 20); do [ "$(stty size)" = "0 0" ] || exec shpool attach -f "$1"; sleep 0.1; done
echo "folio: the terminal has no size, so it wasn't attached" >&2; exit 1
"""
SNAPSHOT = (snapshots.WORKSPACE_ARCHIVE, snapshots.CLAUDE_ARCHIVE, snapshots.WORK)


def work_script(workspace: str) -> str:
    """Lists a workspace's own branches and whether it has uncommitted changes,
    one per line, tab-separated so a name is never split."""
    workspace = shlex.quote(workspace)
    return f"""\
cd {workspace} 2>/dev/null && git rev-parse --git-dir >/dev/null 2>&1 || exit 0
git for-each-ref --format='%(refname:short)%09%(objectname)' refs/heads
git symbolic-ref -q HEAD >/dev/null || printf '(detached HEAD)\\t%s\\n' "$(git rev-parse HEAD)"
[ -z "$(git status --porcelain)" ] || printf '(uncommitted changes)\\n'
"""


@dataclass(frozen=True)
class Thread:
    name: str
    bottle: str
    created: float
    last_active: float
    base: str | None = None  # the branch it started from, if one was asked for; None: the repo's default


def directory(repo: Repo, name: str) -> Path:
    return repo.directory / name


def exists(repo: Repo, name: str) -> bool:
    return NAME.fullmatch(name) is not None and store.current_slot(directory(repo, name)) is not None


def load(repo: Repo, name: str) -> Thread:
    slot = store.current_slot(directory(repo, name)) if NAME.fullmatch(name) else None
    if slot is None:
        known = ", ".join(t.name for t in all_(repo)) or "none"
        raise FolioError(f"no thread named {name!r} in {repo.path} (threads: {known})")
    return Thread(**json.loads((slot / RECORD).read_text()))


def all_(repo: Repo) -> list[Thread]:
    """The repo's threads, most recently active first."""
    threads = []
    for entry in repo.directory.iterdir() if repo.directory.is_dir() else ():
        slot = store.current_slot(entry) if entry.is_dir() else None
        if slot is not None and (slot / RECORD).exists():
            threads.append(Thread(**json.loads((slot / RECORD).read_text())))
    return sorted(threads, key=lambda t: t.last_active, reverse=True)


def _save(repo: Repo, thread: Thread, files: dict[str, Path | None] | None = None) -> None:
    record = (json.dumps(asdict(thread), indent=2) + "\n").encode()
    store.write_slot(directory(repo, thread.name), {RECORD: record, **(files or {})})


def snapshot_size(repo: Repo, thread: Thread) -> int | None:
    """The bytes a paused thread's snapshot takes up; None if it has none."""
    slot = _snapshot(repo, thread)
    return sum((slot / name).stat().st_size for name in SNAPSHOT if (slot / name).exists()) if slot else None


def _snapshot(repo: Repo, thread: Thread) -> Path | None:
    """The slot holding the thread's snapshot, if it has one."""
    slot = store.current_slot(directory(repo, thread.name))
    return slot if slot is not None and (slot / snapshots.WORKSPACE_ARCHIVE).exists() else None


def create(repo: Repo, name: str, base: str | None = None) -> Thread:
    """A new thread: its bottle and its lane. Nothing runs in it until something attaches."""
    if not NAME.fullmatch(name):
        raise FolioError(f"invalid thread name {name!r}: use lowercase letters and digits, in words joined by '-'")
    if store.current_slot(directory(repo, name)) is not None:
        raise FolioError(f"thread {name} already exists; attach with `folio claude {name}`")
    now = time.time()
    thread = Thread(name, f"{repo.key}-{name}", now, now, base)
    _make_bottle(repo, thread)
    try:
        lanes.register(repo.path, name)
        _save(repo, thread)
    except BaseException:
        _undo(repo, thread)
        raise
    return thread


def _make_bottle(repo: Repo, thread: Thread) -> None:
    """Create the thread's bottle from the repo's spec as it is now, served the thread's lane."""
    # Checked again each time: a feature can have changed since the spec was saved.
    spec = engine.validate_spec(repo.spec() or DEFAULT_SPEC)
    repo.remember()
    if repo.spec() != spec:
        repo.save_spec(spec)
    engine.new(thread.bottle, spec, repo.path, lanes.bottle_remotes(thread.name), thread.base,
               advice(thread.name, repo.name))


def _undo(repo: Repo, thread: Thread) -> None:
    """What create did, undone, as far as it got."""
    for step in (lambda: engine.delete(thread.bottle), lambda: lanes.forget(repo.path, thread.name),
                 lambda: shutil.rmtree(directory(repo, thread.name), ignore_errors=True)):
        try:
            step()
        except Exception:
            pass


def states(repo: Repo, threads: list[Thread]) -> dict[str, dict]:
    """Each thread's bottle's status row, by thread name; a missing bottle with a snapshot is paused."""
    rows = engine.status([t.bottle for t in threads])
    result = {}
    for t in threads:
        row = rows.get(t.bottle, {"name": t.bottle, "state": "missing"})
        if row["state"] == "missing" and _snapshot(repo, t) is not None:
            row = {**row, "state": "paused"}
        result[t.name] = row
    return result


def attach(repo: Repo, thread: Thread, session: str) -> NoReturn:
    """Attach this terminal to one of the thread's sessions, bringing the thread to running first."""
    _ensure_running(repo, thread)
    _ensure_session(thread, session)
    # The bottle has the work now, so a snapshot would only be out of date.
    _save(repo, Thread(**{**asdict(thread), "last_active": time.time()}), dict.fromkeys(SNAPSHOT))
    # -f detaches anything else on it: closing a terminal doesn't hang up its
    # attach inside the VM.
    engine.attach(thread.bottle, ["sh", "-c", ATTACH, "attach", session])


def _ensure_running(repo: Repo, thread: Thread) -> None:
    state = states(repo, [thread])[thread.name]["state"]
    if state == "paused":
        print(f"Resuming thread {thread.name}...", file=sys.stderr)
        _resume(repo, thread)
    elif state == "missing":
        raise FolioError(f"thread {thread.name}'s bottle, {thread.bottle}, is gone, and with it everything that "
                         f"wasn't pushed. What was pushed is in your repo: `git fetch bottle/{thread.name}`. "
                         f"`folio delete {thread.name}` removes what's left of the thread")
    elif state != "running":
        engine.start(thread.bottle)


def pause(repo: Repo, thread: Thread) -> None:
    """Snapshot the thread and delete its bottle: everything but its running processes is kept.

    The sessions are stopped first, and waited for, so Claude has written its
    conversation out. Nothing is deleted until both archives are in the
    thread's slot and read back whole; if anything fails before that, the
    bottle is left as it is.
    """
    state = states(repo, [thread])[thread.name]["state"]
    if state == "paused":
        raise FolioError(f"thread {thread.name} is already paused; `folio claude {thread.name}` resumes it")
    if state == "missing":
        raise FolioError(f"thread {thread.name}'s bottle, {thread.bottle}, is gone; there's nothing to pause")
    if state != "running":
        engine.start(thread.bottle)
    _stop_sessions(thread)
    staging = store.staging(directory(repo, thread.name))
    try:
        workspace, claude, work = (staging / name for name in SNAPSHOT)
        engine.run_to_file(thread.bottle, ["sh", "-c", snapshots.workspace_out(WORKSPACE)], workspace)
        engine.run_to_file(thread.bottle, ["sh", "-c", snapshots.CLAUDE_OUT], claude)
        engine.run_to_file(thread.bottle, ["sh", "-c", work_script(WORKSPACE)], work)
        snapshots.check(workspace)
        snapshots.check(claude)
        _save(repo, thread, {snapshots.WORKSPACE_ARCHIVE: workspace, snapshots.CLAUDE_ARCHIVE: claude,
                             snapshots.WORK: work})
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    engine.delete(thread.bottle)


def _stop_sessions(thread: Thread) -> None:
    """Hang up the bottle's sessions, and wait (a while) for them to exit.

    The hangup is folio's own: `shpool kill` follows its SIGHUP with a SIGKILL
    half a second later, too soon for Claude to have written its conversation out.
    """
    pids = [str(s["shell_pid"]) for s in _sessions(thread)]
    if pids:
        engine.run(thread.bottle, ["kill", "-HUP", *pids], check=False)
        engine.run(thread.bottle, ["sh", "-c", f"for i in $(seq 50); do alive=; for p in {' '.join(pids)}; do "
                                               "kill -0 $p 2>/dev/null && alive=1; done; "
                                               '[ -z "$alive" ] && exit 0; sleep 0.2; done'])


def _resume(repo: Repo, thread: Thread) -> None:
    """A new bottle for a paused thread, with its snapshot restored; on any failure, still paused."""
    slot = _snapshot(repo, thread)
    lanes.register(repo.path, thread.name)  # repairs it, if it was edited meanwhile
    _make_bottle(repo, thread)
    try:
        engine.run_from_file(thread.bottle, ["sh", "-c", snapshots.workspace_in(WORKSPACE)],
                             slot / snapshots.WORKSPACE_ARCHIVE)
        engine.run_from_file(thread.bottle, ["sh", "-c", snapshots.CLAUDE_IN], slot / snapshots.CLAUDE_ARCHIVE)
    except BaseException:
        try:
            engine.delete(thread.bottle)
        except Exception:
            pass
        raise


def _sessions(thread: Thread) -> list[dict]:
    """The bottle's shpool sessions, as `shpool list --json` has them."""
    listed = engine.run(thread.bottle, ["shpool", "list", "--json"], check=False)
    return json.loads(listed.stdout)["sessions"] if listed.returncode == 0 else []


def _ensure_session(thread: Thread, session: str) -> None:
    listed = next((s for s in _sessions(thread) if s["name"] == session), None)
    if listed is not None:
        if engine.run(thread.bottle, ["kill", "-0", str(listed["shell_pid"])], check=False).returncode == 0:
            return
        # Its program exited, but shpool keeps the session, and attaching to it
        # would start a plain shell in its place.
        engine.run(thread.bottle, ["shpool", "kill", session], check=False)
    engine.run(thread.bottle, ["env", f"TERM={TERM}", "shpool", "attach", "--background", "--dir", WORKSPACE,
                               "--cmd", shlex.join(["sh", "-c", SESSIONS[session]]), session])


def losses(repo: Repo, thread: Thread) -> list[str]:
    """What deleting the thread would lose, for the user to read first.

    Branches in the lane that aren't on the user's branches or origin's, and
    work that never reached the lane: read from the bottle, which is started
    if it's stopped, or from a paused thread's snapshot.
    """
    lost = [f"{branch}: pushed, but not on your branches or origin's" for branch in lanes.unintegrated(repo.path, thread.name)]
    state = states(repo, [thread])[thread.name]["state"]
    if state == "missing":
        return lost
    if state == "paused":
        # As the bottle saw it at pause: the snapshot itself is never opened here (see snapshots.py).
        work = (_snapshot(repo, thread) / snapshots.WORK).read_text()
    else:
        if state != "running":
            engine.start(thread.bottle)
        work = engine.run(thread.bottle, ["sh", "-c", work_script(WORKSPACE)]).stdout
    tips, dirty = {}, False
    for line in work.splitlines():
        name, _, commit = line.partition("\t")
        if name == "(uncommitted changes)":
            dirty = True
        elif commit:
            tips[name] = commit
    where = "the snapshot" if state == "paused" else "the bottle"
    lost += [f"{name}: in {where}, never pushed" for name in lanes.unsaved(repo.path, thread.name, tips)]
    if dirty:
        lost.append(f"uncommitted changes in {where}")
    return lost


def delete(repo: Repo, thread: Thread) -> None:
    """Delete the thread: its bottle, its lane and remote, and its record and snapshot."""
    if states(repo, [thread])[thread.name]["state"] not in ("missing", "paused"):
        engine.delete(thread.bottle)
    lanes.forget(repo.path, thread.name)
    shutil.rmtree(directory(repo, thread.name))
