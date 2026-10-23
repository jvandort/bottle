"""Asking the person things, when there is a person to ask."""

import os
import sys

from .gitcmd import git, rev, run
from .pick import Row, choose
from .state import CLEAN_PREFIX, named, short

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


def default_base(head: str) -> tuple[str, str, str] | tuple[None, None, None]:
    """Where this branch left the line it came from, if that can be worked out.

    Only ever a suggestion, offered as the default answer and never taken
    silently: where the clean line starts decides what there is to review.
    """
    for candidate in (f"{short(head)}@{{upstream}}", "origin/HEAD", "origin/main",
                      "origin/master", "main", "master"):
        other = rev(candidate)
        if other is None or other == rev(head):
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


def ask_for_base(head: str) -> str:
    return pick_base(head, *default_base(head))


def decorations(refs: str) -> str:
    """%D without curate's own refs and without HEAD: what a person names."""
    names = []
    for name in refs.split(", "):
        name = name.removeprefix("HEAD -> ")
        if name and name != "HEAD" and not name.startswith(f"{CLEAN_PREFIX}/"):
            names.append(name)
    return ", ".join(names)


def history(tip: str, guess: str | None) -> list[tuple[str, str, str]]:
    """(oid, subject, refs) down the first-parent line from `tip`, as far as
    a little past the guess."""
    out = git("log", "--first-parent", f"-n{LONGEST_LIST}",
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
        subject = git("log", "-1", "--format=%s", guess)
        found.append((guess, subject, ""))
    return found


def pick_base(head: str, guess: str | None, via: str | None,
              tip: str | None, header: list[str] | None = None) -> str:
    rows: list[Row] = []
    cursor = 0
    for oid, subject, refs in history(head, guess):
        if oid == guess:
            cursor = len(rows)
        named_here = f"({refs}) {subject}" if refs else subject
        rows.append(Row([oid[:8], named_here], value=oid,
                        tag="best guess" if oid == guess else ""))
    lines = list(header or [])
    if guess and via and tip:
        lines.append(suggestion(head, guess, via, tip))
    lines += [f"Clean line for {named(head)} starts at:", ""]

    def typed(text: str) -> tuple[str | None, str]:
        found = rev(text)
        return (found, "") if found else (None, f"cannot resolve {text}")

    base = choose(lines, rows, cursor=cursor, typed=typed)
    print(f"Clean line starts at {base[:8]} "
          f"{git('log', '-1', '--format=%s', base)}")
    return base
