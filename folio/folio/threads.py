"""Threads: one line of work in one repo, and the bottle it's being worked in.

A thread has a kebab-case name, unique within its repo; its bottle is named
`<repo key>-<thread>`, unique across repos. Its record is in its slot (see
store.py), and its state is read live from bottle, never trusted from the
record:

    running   the bottle is up: attach to it
    stopped   the VM stopped, its disk intact (a crash, a restart, `bottle
              stop`): start it, and the work is all there
    missing   the bottle is gone

Inside the bottle, Claude and a shell each run in a tmux session of their own,
started by one exec and attached to by another, so closing a terminal only
detaches it, and whatever the session started keeps running.
"""

import json
import re
import shutil
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import NoReturn

from . import engine, lanes, store
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
# A bottle's own branches and uncommitted changes, one per line, tab-separated
# so a name is never split.
WORK_SCRIPT = f"""\
cd {WORKSPACE} 2>/dev/null && git rev-parse --git-dir >/dev/null 2>&1 || exit 0
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


def _save(repo: Repo, thread: Thread) -> None:
    store.write_slot(directory(repo, thread.name), {RECORD: (json.dumps(asdict(thread), indent=2) + "\n").encode()})


def create(repo: Repo, name: str, base: str | None = None) -> Thread:
    """A new thread: its bottle and its lane. Nothing runs in it until something attaches."""
    if not NAME.fullmatch(name):
        raise FolioError(f"invalid thread name {name!r}: use lowercase letters and digits, in words joined by '-'")
    if store.current_slot(directory(repo, name)) is not None:
        raise FolioError(f"thread {name} already exists; attach with `folio claude {name}`")
    # Checked again each time: a feature can have changed since the spec was saved.
    spec = engine.validate_spec(repo.spec() or DEFAULT_SPEC)
    repo.remember()
    if repo.spec() != spec:
        repo.save_spec(spec)
    bottle = f"{repo.key}-{name}"
    engine.new(bottle, spec, repo.path, lanes.bottle_remotes(name), base, advice(name, repo.name))
    now = time.time()
    thread = Thread(name, bottle, now, now, base)
    try:
        lanes.register(repo.path, name)
        _save(repo, thread)
    except BaseException:
        _undo(repo, thread)
        raise
    return thread


def _undo(repo: Repo, thread: Thread) -> None:
    """What create did, undone, as far as it got."""
    for step in (lambda: engine.delete(thread.bottle), lambda: lanes.forget(repo.path, thread.name),
                 lambda: shutil.rmtree(directory(repo, thread.name), ignore_errors=True)):
        try:
            step()
        except Exception:
            pass


def states(threads: list[Thread]) -> dict[str, dict]:
    """Each thread's bottle's status row, by thread name."""
    rows = engine.status([t.bottle for t in threads])
    return {t.name: rows.get(t.bottle, {"name": t.bottle, "state": "missing"}) for t in threads}


def attach(repo: Repo, thread: Thread, session: str) -> NoReturn:
    """Attach this terminal to one of the thread's sessions, bringing the thread to running first."""
    _ensure_running(thread)
    _ensure_session(thread, session)
    _save(repo, Thread(**{**asdict(thread), "last_active": time.time()}))
    # -d detaches anything else on it: closing a terminal doesn't hang up its
    # attach inside the VM, and tmux sizes a window to its smallest client.
    engine.attach(thread.bottle, ["tmux", "attach", "-d", "-t", f"={session}"])


def _ensure_running(thread: Thread) -> None:
    state = states([thread])[thread.name]["state"]
    if state == "missing":
        raise FolioError(f"thread {thread.name}'s bottle, {thread.bottle}, is gone; its branches are safe in "
                         f"bottle/{thread.name}, and `folio delete {thread.name}` removes the rest")
    if state != "running":
        engine.start(thread.bottle)


def _ensure_session(thread: Thread, session: str) -> None:
    if engine.run(thread.bottle, ["tmux", "has-session", "-t", f"={session}"], check=False).returncode == 0:
        return
    engine.run(thread.bottle, ["tmux", "new-session", "-d", "-s", session, "-c", WORKSPACE, SESSIONS[session]])


def losses(repo: Repo, thread: Thread) -> list[str]:
    """What deleting the thread would lose, for the user to read first.

    Branches in the lane that aren't on the user's branches or origin's, and
    work in the bottle that never reached the lane. A stopped bottle is
    started to look.
    """
    lost = [f"{branch}: pushed, but not on your branches or origin's" for branch in lanes.unintegrated(repo.path, thread.name)]
    state = states([thread])[thread.name]["state"]
    if state == "missing":
        return lost
    if state != "running":
        engine.start(thread.bottle)
    tips, dirty = {}, False
    for line in engine.run(thread.bottle, ["sh", "-c", WORK_SCRIPT]).stdout.splitlines():
        name, _, commit = line.partition("\t")
        if name == "(uncommitted changes)":
            dirty = True
        elif commit:
            tips[name] = commit
    lost += [f"{name}: in the bottle, never pushed" for name in lanes.unsaved(repo.path, thread.name, tips)]
    if dirty:
        lost.append("uncommitted changes in the bottle")
    return lost


def delete(repo: Repo, thread: Thread) -> None:
    """Delete the thread: its bottle, its lane and remote, and its record."""
    if states([thread])[thread.name]["state"] != "missing":
        engine.delete(thread.bottle)
    lanes.forget(repo.path, thread.name)
    shutil.rmtree(directory(repo, thread.name))
