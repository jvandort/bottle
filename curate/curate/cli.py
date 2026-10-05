"""What you can type, and what happens when you do."""

import sys
from typing import Callable

from .commands import cmd_drop, cmd_list, cmd_status
from .errors import Refused
from .fixup import cmd_fixup, cmd_resolve
from .gitverb import cmd_git
from .hooks import cmd_hook, install as install_hooks
from .modes import NoReview, switch_mode
from .start import cmd_start
from .state import STATE_VERSION, VERSION, Repo
from .ui import interactive

HELP = """  curate start --from <base>   begin. The clean line starts at <base>; the
                               branch you are on is the working line.
  curate review                read your files against the clean line. Stage a
                               hunk to approve it, commit to ship it.
  curate write                 back to an ordinary repository, to write code.
  curate git <command>         run a git command as if you were in write mode,
                               whatever mode you are in, then come back. So
                               `curate git pull` without leaving the review.
  curate switch                toggle between the two.
  curate status                which mode you are in, the two branches, and
                               anything wrong that curate cannot put right.
                               For what is unreviewed, use `git status`.
  curate fixup <commit>        fold what you have approved into an existing
                               clean commit, instead of appending a fix to it.
                               --continue / --abort when it conflicts.
  curate resolve               open the current fixup conflict in git's merge tool.
  curate list                  every review in this repository.
  curate drop <branch>         forget a review started by `start`.

In review mode your editor does the work: staging is approving, and committing
extends the clean line. Unstaged is your review to-do list, so the review
is finished when `git status` is empty."""


# The dispatch table, out here rather than inside main, so that
# tests/test_help.py can check HELP against it. `hook` is git's to call, not
# yours, which is why it is listed apart.
COMMANDS: dict[str, Callable[[Repo, list[str]], int]] = {
    "start": cmd_start,
    "git": cmd_git,
    "status": cmd_status,
    "fixup": cmd_fixup,
    "resolve": cmd_resolve,
    "list": cmd_list,
    "drop": cmd_drop,
}
MODE_VERBS = ("write", "review", "switch")
INTERNAL = {"hook": cmd_hook}


def main(argv: list[str]) -> int:
    if argv and argv[0] in ("-h", "--help", "help"):
        print(HELP)
        return 0
    if argv and argv[0] in ("-V", "--version", "version"):
        print(f"curate {VERSION} (state version {STATE_VERSION})")
        return 0
    try:
        repo = Repo()
    except Refused as e:
        if not argv:
            # Nothing here to say anything about. Say only that.
            print("curate only works inside a git repository. Run it in one.")
            return 0
        print(f"curate: {e}", file=sys.stderr)
        return 1

    verb_now = argv[0] if argv else ""
    try:
        # Hooks stay quiet rather than printing on every `git add`.
        if verb_now != "hook":
            repo.check_state_version()
    except Refused as e:
        print(f"curate: {e}", file=sys.stderr)
        return 1

    # Hooks hold an absolute path to this program, which a rename or a move
    # invalidates. Rewriting them whenever they are wrong costs a few reads and
    # means a stale one never outlives the next command.
    if repo.state.is_dir() and verb_now != "hook":
        install_hooks(repo, quiet=True)

    if not argv:
        # Bare `curate` answers "where am I?" if there is an answer, and
        # explains itself if there is not.
        if repo.session() is None:
            print(HELP)
            print("\nNo review in this repository yet. "
                  "`curate start --from <base>` begins one.")
            return 0
        return cmd_status(repo, [])
    verb, rest = argv[0], argv[1:]

    try:
        if verb in MODE_VERBS:
            to = verb if verb != "switch" else (
                "write" if repo.mode == "review" else "review")
            try:
                return switch_mode(repo, to)
            except NoReview:
                # Nothing to switch into, but something to offer.
                if not interactive():
                    raise
                print("No review in this repository yet; starting one.")
                cmd_start(repo, [])
                return switch_mode(repo, to)
        if verb not in COMMANDS and verb not in INTERNAL:
            raise Refused(f"no such command: {verb}. `curate --help` lists them.")
        if verb in INTERNAL:
            return INTERNAL[verb](repo, rest)
        return COMMANDS[verb](repo, rest)
    except Refused as e:
        print(f"curate: {e}", file=sys.stderr)
        return 1
    except BrokenPipeError:
        return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
