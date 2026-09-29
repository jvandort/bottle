"""The three hooks, and what they do when git fires them.

`post-index-change` is what makes an approval durable the instant it is made;
`post-commit` does the same for a commit, and `post-checkout` stands a review
down when HEAD leaves it, so that switching away needs no `curate write`.
"""

import os
import sys

from .gitcmd import HOOK_GUARD
from .state import Repo, read

HOOKS = ("post-index-change", "post-checkout", "post-commit")
MARKER = "# installed by curate; safe to delete"


def script(name: str, me: str) -> str:
    """The hook, which has to survive curate moving or going away.

    It holds an absolute path, so a rename, a move or a deleted checkout all
    leave it pointing at nothing. The guard is what keeps that quiet: without
    it, git prints an interpreter error on every commit until someone works
    out where it is coming from.
    """
    return (f"#!/bin/sh\n{MARKER}\n"
            f'CURATE="{me}"\n'
            f'[ -f "$CURATE" ] || exit 0        # curate moved or went away\n'
            f'"{sys.executable}" "$CURATE" hook {name} "$@" || true\n'
            f"exit 0\n")


def me() -> str:
    """The absolute path a hook has to name to reach this curate."""
    return os.path.realpath(sys.argv[0])


def foreign(repo: Repo) -> list[str]:
    """Hooks curate is not installed in, because you already had one there.

    `install` leaves them alone and says so once. Once is not enough: without
    `post-index-change` an approval is only durable at the next command rather
    than the instant it is made, and that is exactly the kind of quiet
    degradation you want told about every time you ask what is wrong.
    """
    hooks = repo.gitdir / "hooks"
    return [name for name in HOOKS
            if (hooks / name).exists() and MARKER not in read(hooks / name)]


def install(repo: Repo, quiet: bool = False) -> list[str]:
    """Write the hooks, or rewrite them if they name a curate that has moved.

    Called from `start`, and again on every command, so a stale path repairs
    itself the next time you run anything.
    """
    hooks = repo.gitdir / "hooks"
    hooks.mkdir(parents=True, exist_ok=True)
    written = []
    for name in HOOKS:
        path = hooks / name
        wanted = script(name, me())
        if path.exists():
            current = read(path)
            if MARKER not in current:
                if not quiet:
                    print(f"curate: leaving your existing {name} hook alone; "
                          f"add `{me()} hook {name} \"$@\"` to it by hand.",
                          file=sys.stderr)
                continue
            if current.strip() == wanted.strip():
                continue
        path.write_text(wanted)
        path.chmod(0o755)
        written.append(name)
    return written


def cmd_hook(repo: Repo, argv: list[str]) -> int:
    if os.environ.get(HOOK_GUARD) == "1":
        return 0
    name, args = argv[0], argv[1:]
    session = repo.session()
    if session is None:
        return 0

    if name == "post-index-change":
        # args are <updated_workdir> <updated_skipworktree>. A checkout updates
        # the working directory too, and fires this hook before HEAD has
        # necessarily moved -- so that flag is the only thing that tells an
        # approval apart from being yanked out. Recording then would overwrite
        # the approved set with whatever was just checked out.
        if args[:1] == ["1"]:
            return 0
        live = os.environ.get("GIT_INDEX_FILE")
        if live and os.path.realpath(live) != os.path.realpath(repo.gitdir / "index"):
            return 0
        if repo.mode == "review" and repo.head == session.clean:
            session.record("review")
        return 0

    if name == "post-checkout":
        # Runs afterwards, so it cannot prevent anything, and need not: git
        # has already rewritten the live index, and the ref holds the rest.
        flag = args[2] if len(args) > 2 else "1"
        if flag != "1" or repo.consistent():
            return 0
        if repo.mode == "review":
            # Standing down here rather than at the next command is what makes
            # the next `git switch` back to the working line ordinary.
            repo.recover()
        return 0

    if name == "post-commit":
        if repo.mode != "review" or repo.head != session.clean:
            return 0
        session.record("review")
        return 0
    return 0
