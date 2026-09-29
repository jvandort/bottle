"""Every call this tool makes to git.

Almost all of it is plumbing, whose output is a stable interface. The two
porcelain commands it relies on are the user's: `git add -p` and, in review
mode, `git commit`.
"""

import os
import subprocess
from pathlib import Path

from .errors import Refused

HOOK_GUARD = "CURATE_IN_HOOK"

# What merge-tree hands back for a conflicted path: stage -> (mode, oid),
# where stage 1 is the merge base, 2 is ours and 3 is theirs.
Stages = dict[int, tuple[str, str]]
Conflicts = dict[str, Stages]


def environment(index: str | Path | None = None,
                env: dict[str, str] | None = None) -> dict[str, str]:
    """The environment every git we run gets. GIT_INDEX_FILE is ours to set,
    never inherited."""
    e = dict(os.environ)
    e.pop("GIT_INDEX_FILE", None)
    e[HOOK_GUARD] = "1"          # git we run must not re-enter our own hooks
    if index is not None:
        e["GIT_INDEX_FILE"] = str(index)
    if env:
        e.update(env)
    return e


def run(args: list[str], index: str | Path | None = None,
        env: dict[str, str] | None = None,
        stdin: str | None = None) -> subprocess.CompletedProcess[str]:
    """Run git and decode its output as text."""
    return subprocess.run(["git", *args], capture_output=True, text=True,
                          env=environment(index, env), input=stdin)


def git(*args: str, index: str | Path | None = None,
        env: dict[str, str] | None = None, check: bool = True) -> str:
    p = run(list(args), index=index, env=env)
    if check and p.returncode != 0:
        raise Refused(f"git {' '.join(args)} failed: {p.stderr.strip()}")
    return p.stdout.strip()


def git_ok(*args: str, index: str | Path | None = None,
           env: dict[str, str] | None = None) -> bool:
    return run(list(args), index=index, env=env).returncode == 0


def blob(spec: str) -> bytes | None:
    """A blob's exact bytes, or None if it is not there.

    Never `git()` for file content: that strips, which silently reindents the
    first line and drops trailing blank lines. File content is bytes and is
    handled as bytes the whole way -- see `fixup.materialise`.
    """
    p = subprocess.run(["git", "cat-file", "blob", spec], capture_output=True,
                       env=environment())
    return p.stdout if p.returncode == 0 else None


def rev(spec: str) -> str | None:
    """Resolve a rev, or None."""
    p = run(["rev-parse", "--verify", "-q", f"{spec}^{{}}"])
    return p.stdout.strip() if p.returncode == 0 else None


def tree_of(spec: str) -> str | None:
    p = run(["rev-parse", "--verify", "-q", f"{spec}^{{tree}}"])
    return p.stdout.strip() if p.returncode == 0 else None


def parents_of(commit: str) -> list[str]:
    return git("rev-list", "--parents", "-n", "1", commit).split()[1:]


def metadata(commit: str) -> tuple[dict[str, str], str]:
    """Author identity and message of a commit, for rebuilding it faithfully.

    Forgetting this silently rewrites authorship, which nobody notices for a
    month. The committer is deliberately whoever ran the fixup.
    """
    out = git("log", "-1", "--format=%an%n%ae%n%aI%n%B", commit)
    name, email, date, message = (out.split("\n", 3) + ["", "", "", ""])[:4]
    return ({"GIT_AUTHOR_NAME": name,
             "GIT_AUTHOR_EMAIL": email,
             "GIT_AUTHOR_DATE": date}, message)


def commit_tree(tree: str, parents: list[str], message: str,
                env: dict[str, str] | None = None) -> str:
    args = ["commit-tree", tree]
    for p in parents:
        args += ["-p", p]
    return git(*args, "-m", message, env=env)


def merge_tree(base: str, ours: str,
               theirs: str) -> tuple[str, Conflicts | None]:
    """Three-way merge in the object database. Returns (tree, conflicts).

    conflicts is None on success, otherwise {path: {stage: (mode, oid)}} and
    the tree is the one with ordinary conflict markers in it.
    """
    p = run(["merge-tree", "--write-tree", f"--merge-base={base}", ours, theirs])
    lines = p.stdout.splitlines()
    if p.returncode == 0:
        return lines[0], None
    if p.returncode != 1:
        raise Refused(f"merge-tree failed: {p.stderr.strip() or p.stdout.strip()}")
    tree: str = lines[0]
    conflicts: Conflicts = {}
    for line in lines[1:]:
        if not line.strip():
            break
        info, _, path = line.partition("\t")
        mode, oid, stage = info.split()
        conflicts.setdefault(path, {})[int(stage)] = (mode, oid)
    return tree, conflicts
