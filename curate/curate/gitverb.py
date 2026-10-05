"""The `git` verb: run a git command as if you were in write mode.

In review mode HEAD is the clean line, where half of git is beside the point:
the clean line has no upstream to pull from, its log is not the branch's, and
`git status` is the review rather than a report on your work. So catching up
with the working line is `curate write`, the command, `curate review` -- three
commands to run one, and the two on the outside are bookkeeping.

This is those three as one. It borrows write mode for exactly as long as the
command takes, and puts the review back afterwards however the command went:
failure, a signal, a pager you quit out of. The review comes back because the
approved set is content, and running git on the working line does not change
what you already read.
"""

import subprocess
import sys

from .errors import Refused
from .gitcmd import HOOK_GUARD, environment
from .modes import switch_mode
from .state import Repo, Session, named, short


def run_git(argv: list[str]) -> int:
    """Hand the command to git, with our stdio, and report how it went.

    Not `gitcmd.run`: that captures output and is for git we read. This one is
    the user's own command -- it gets the terminal, so a pager pages, `add -p`
    prompts and a colour is still a colour.
    """
    env = environment()
    # environment() guards against re-entering our own hooks, which is right
    # for the git curate runs and wrong here: this is your git, and in write
    # mode curate's hooks have things to tell you -- `curate git switch
    # elsewhere` is exactly when post-checkout should speak up.
    env.pop(HOOK_GUARD, None)
    sys.stdout.flush()
    sys.stderr.flush()
    code = subprocess.run(["git", *argv], env=env).returncode
    # A signal comes back negative; shells report those as 128 + n, and so do we.
    return 128 - code if code < 0 else code


def restore(repo: Repo, session: Session) -> None:
    """Back to review mode, unless the command moved HEAD out from under us.

    A checkout inside the borrowed write mode is legal and sometimes the point,
    but it means the working line is no longer where you are standing, so there
    is no review to return to. Say so rather than fail the switch.
    """
    if repo.head == session.working:
        switch_mode(repo, "review")
        return
    print(f"curate: HEAD is {named(repo.head)} now, not {named(session.working)}, "
          f"so there was no\n        review to come back to. Your command ran, "
          f"and your approvals are safe in\n        {session.ref}.\n"
          f"To get back:  git switch {short(session.working)} && curate review",
          file=sys.stderr)


def cmd_git(repo: Repo, argv: list[str]) -> int:
    if not argv:
        raise Refused("usage: curate git <command> [args...]")

    repo.recover()
    repo.require_consistent()
    session = repo.session()
    if session is None or repo.mode != "review":
        # Already an ordinary repository -- write mode, or no review at all --
        # so `curate git x` is `git x`. The point of it working here too is
        # that you never have to know which mode you were in to type it.
        return run_git(argv)

    switch_mode(repo, "write")
    try:
        return run_git(argv)
    finally:
        # However it went, and including a Ctrl-C out of a pager: leaving you
        # on the clean line with the working line's index would be the one
        # state curate exists to prevent.
        restore(repo, session)
