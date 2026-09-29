"""Asking the person things, when there is a person to ask."""

import sys

from .errors import Refused
from .gitcmd import git, rev, run
from .state import named, short


def interactive() -> bool:
    """Whether there is a person here to answer a question."""
    try:
        return sys.stdin.isatty() and sys.stdout.isatty()
    except (AttributeError, ValueError):
        return False


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


def ask_for_base(head: str) -> str:
    base, via, tip = default_base(head)
    if base:
        if base == tip:
            # The branch itself, so name it: that is what you want to recognise.
            print(f"Suggested: {named(via)} ({base[:8]})")
            label = via
        else:
            behind = git("rev-list", "--count", f"{base}..{tip}", check=False)
            plural = "" if behind == "1" else "s"
            print(f"Suggested: {base[:8]}, where {named(head)} left {named(via)} "
                  f"({named(via)} has {behind} commit{plural} since)")
            label = base[:8]
        prompt = f"Clean line starts at [{label}]: "
    else:
        prompt = "Clean line starts at: "
    while True:
        try:
            answer = input(prompt).strip()
        except (EOFError, KeyboardInterrupt):
            print()
            raise Refused("cancelled; nothing was created")
        if not answer:
            if base:
                return base
            continue
        resolved = rev(answer)
        if resolved is not None:
            return resolved
        print(f"  cannot resolve {answer}")
