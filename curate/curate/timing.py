"""How long each command took, and where the time went.

Every command appends one line to `.git/curate/log`: when it ran, what it was,
how long it took, and how much of that was git. A command slower than
CURATE_SLOW seconds (default 1) also lists every subprocess it ran, in order,
with when each started and how long it took. Time spent waiting for you to
answer a question is listed too, but apart: it is not curate being slow. So when one feels slow, the log
already says why, without having to reproduce it.

Always on, because the slow ones are the ones you did not expect. It costs a
clock read per subprocess and one append per command. Nothing is written until
a review has created `.git/curate`, and logging never creates it: whether that
directory exists is what tells curate there is anything here at all.

Hooks are logged like any other command, since a `git add` that fires one is
exactly where time goes unnoticed. The ones git fires under curate's own git
are not: they return at once, and their cost shows up in the call that fired
them.
"""

import os
import shlex
import time
from contextlib import contextmanager
from datetime import datetime
from pathlib import Path
from typing import Iterator

LOG_LIMIT = 1 << 20              # past this, the log becomes log.1 and starts over

_began = time.perf_counter()
_when = datetime.now().astimezone()                 # when the command started
_calls: list[tuple[float, float, list[str]]] = []      # (start, duration, argv)
_state: Path | None = None


def log_into(state: Path) -> None:
    """Where the log goes: this repository's .git/curate, once there is one."""
    global _state
    _state = state


WAITING = ["(waiting for you)"]     # what `waiting` logs in place of a command


@contextmanager
def waiting() -> Iterator[None]:
    """Time a question, which is the person and not curate: logged in the
    list like a subprocess, kept out of the subprocess total, and not counted
    towards whether the command was slow."""
    with timed(WAITING):
        yield


@contextmanager
def timed(argv: list[str]) -> Iterator[None]:
    """Time one subprocess, for the log."""
    start = time.perf_counter()
    try:
        yield
    finally:
        _calls.append((start - _began, time.perf_counter() - start, argv))


def slow() -> float:
    try:
        return float(os.environ.get("CURATE_SLOW", "1"))
    except ValueError:
        return 1.0


def entry(argv: list[str], code: int | str) -> str:
    total = time.perf_counter() - _began
    waited = sum(d for _, d, a in _calls if a is WAITING)
    run = [d for _, d, a in _calls if a is not WAITING]
    when = _when.isoformat(timespec="milliseconds")
    asked = f"waited for you {waited:.3f}s; " if waited else ""
    lines = [f"{when} pid {os.getpid()} {total:.3f}s "
             f"({asked}subprocesses {sum(run):.3f}s in {len(run)}) exit {code}: "
             f"curate {shlex.join(argv)}"]
    if total - waited >= slow():
        lines += [f"    +{start:.3f}s {d:.3f}s "
                  f"{a[0] if a is WAITING else shlex.join(a)}"
                  for start, d, a in _calls]
    return "\n".join(lines) + "\n"


def flush(argv: list[str], code: int | str) -> None:
    """Append this command's entry. Never fails the command it describes."""
    state = _state
    if state is None or not state.is_dir():
        return
    log = state / "log"
    try:
        if log.exists() and log.stat().st_size > LOG_LIMIT:
            log.replace(state / "log.1")
        # One write on an O_APPEND file, so hooks running alongside cannot
        # interleave inside an entry.
        fd = os.open(log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
        try:
            os.write(fd, entry(argv, code).encode())
        finally:
            os.close(fd)
    except OSError:
        pass
