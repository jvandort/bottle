"""Asking the person things, when there is a person to ask."""

import os
import signal
import sys
import threading
from contextlib import contextmanager
from typing import Iterator

from .gitcmd import Objects, git, run
from .pick import Row, choose
from .state import named, short

PAST_THE_GUESS = 10        # how far the base list goes below the best guess
LONGEST_LIST = 500         # and how far it goes at most


def interactive() -> bool:
    """Whether there is a person here to answer a question, on a terminal
    curses can draw the question on. Anywhere else the answer is a flag."""
    try:
        tty = sys.stdin.isatty() and sys.stdout.isatty()
    except (AttributeError, ValueError):
        return False
    return tty and os.environ.get("TERM", "") not in ("", "dumb")


@contextmanager
def uninterrupted() -> Iterator[None]:
    """Ctrl-C ignored until the block is done, by the git it runs as well.

    For the stretch after every question is answered, where refs move one at a
    time: stopped half way, a command leaves a state that it refuses to pick up
    again. Nothing in such a block waits on a person, so the wait is short.
    """
    if threading.current_thread() is not threading.main_thread():
        yield
        return
    previous = signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        yield
    finally:
        signal.signal(signal.SIGINT, previous)


def default_base(head: str, objects: Objects) -> tuple[str, str, str] | tuple[None, None, None]:
    """Where this branch left the line it came from, if that can be worked out.

    Only ever a suggestion, offered as the default answer and never taken
    silently: where the clean line starts decides what there is to review.
    """
    mine = objects.rev(head)
    for candidate in (f"{short(head)}@{{upstream}}", "origin/HEAD", "origin/main",
                      "origin/master", "main", "master"):
        other = objects.rev(candidate)
        if other is None or other == mine:
            continue
        found = run(["merge-base", head, candidate])
        if found.returncode != 0:
            continue
        # The name a person would recognise: `wip@{upstream}` is not one.
        name = git("rev-parse", "--abbrev-ref", candidate, check=False) or candidate
        return found.stdout.strip(), name, other
    return None, None, None


def suggestion(head: str, base: str, via: str, tip: str) -> str:
    """The best guess in words."""
    if base == tip:
        # The branch itself, so name it: that is what you want to recognise.
        return f"Suggested: {named(via)} ({base[:8]})"
    behind = git("rev-list", "--count", f"{base}..{tip}", check=False)
    plural = "" if behind == "1" else "s"
    return (f"Suggested: {base[:8]}, where {named(head)} left {named(via)} "
            f"({named(via)} has {behind} commit{plural} since)")


def ask_for_base(head: str, objects: Objects, hide: set[str]) -> str:
    return pick_base(head, objects, *default_base(head, objects), hide=hide)


def decorations(refs: str) -> str:
    """%D without HEAD: what a person names."""
    names = []
    for name in refs.split(", "):
        name = name.removeprefix("HEAD -> ")
        if name and name != "HEAD":
            names.append(name)
    return ", ".join(names)


def history(tip: str, objects: Objects, guess: str | None,
            hide: set[str]) -> list[tuple[str, str, str]]:
    """(oid, subject, refs) down the first-parent line from `tip`, as far as
    a little past the guess. `hide` is refs not to name commits by."""
    out = git("log", "--first-parent", f"-n{LONGEST_LIST}",
              *[f"--decorate-refs-exclude={ref}" for ref in sorted(hide)],
              "--format=%H%x1f%s%x1f%D", tip)
    found, past = [], None
    for line in out.splitlines():
        oid, subject, refs = (line.split("\x1f") + ["", ""])[:3]
        found.append((oid, subject, decorations(refs)))
        if oid == guess:
            past = 0
        elif past is not None:
            past += 1
            if past >= PAST_THE_GUESS:
                break
    if guess and past is None:
        # Reached through a merge rather than down the first parent: still
        # the best guess, so it goes in the list, at the end.
        subject = objects.subject(guess)
        found.append((guess, subject, ""))
    return found


def pick_base(head: str, objects: Objects, guess: str | None, via: str | None,
              tip: str | None, header: list[str] | None = None,
              marks: dict[str, str] | None = None,
              hide: set[str] | None = None) -> str:
    """The base from a list. `marks` labels commits besides the guess; `hide`
    is refs it names none of them by -- the clean lines.

    Not the branch's own tip, which would leave nothing to review, when there
    is a better guess. With none -- a branch with nothing of its own yet, whose
    upstream is where it is -- the tip is as good a place as any to start.
    `/` still takes it, as it takes anything."""
    marks = marks or {}
    rows: list[Row] = []
    cursor = 0
    found = history(head, objects, guess, hide or set())
    if guess and found[0][0] != guess:
        found = found[1:]
    for oid, subject, refs in found:
        if oid == guess:
            cursor = len(rows)
        named_here = f"({refs}) {subject}" if refs else subject
        tags = (["best guess"] if oid == guess else []) + (
            [marks[oid]] if oid in marks else [])
        rows.append(Row([oid[:8], named_here], value=oid, tag=", ".join(tags)))
    lines = list(header or [])
    if guess and via and tip:
        lines.append(suggestion(head, guess, via, tip))
    lines += [f"Clean line for {named(head)} starts at:", ""]

    def typed(text: str) -> tuple[str | None, str]:
        found = objects.rev(text)
        return (found, "") if found else (None, f"cannot resolve {text}")

    base = choose(lines, rows, cursor=cursor, typed=typed)
    print(f"Clean line starts at {base[:8]} "
          f"{git('log', '-1', '--format=%s', base)}")
    return base
