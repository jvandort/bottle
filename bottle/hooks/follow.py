"""After a bottle's push: bring the host's checked-out branches up to what it pushed (see bottle/host.py).

Run by hooks/post-receive, on the host, in the host repo's git dir, once the
push's refs have moved. Stdin is receive-pack's `<old> <new> <ref>` lines, the
refs as the bottle named them, inside its namespace (GIT_NAMESPACE). What it
says on stdout reaches the bottle as `remote:` lines in its push's output,
and goes to $BOTTLE_HOOK_LOG for bottled's log.

Two kinds of branch follow a push:

- **Tracking.** A branch of the host's whose upstream is a remote reading this
  namespace -- folio's `bottle/<thread>`, whose uploadpack is `git
  --namespace=<thread>-out upload-pack` -- follows the bottle's branch of the
  same name. That remote is fetched first, so its tracking refs are current
  too. On unless the repo says `git config bottle.follow false`.
- **advance=BRANCH**, a remote's option ($BOTTLE_ADVANCE): the host's BRANCH
  follows the bottle's, tracking or not (folio's broker's main).

Following is `git merge --ff-only`, in whichever worktree has the branch
checked out, and nothing else: a branch with commits of the host's own, a
change in the way, or a worktree in the middle of a merge, rebase,
cherry-pick, revert or bisect is left as it is, and the bottle is told why.
A branch checked out nowhere -- under a curate review, say, where HEAD is
curate/<branch> -- isn't touched. The repo's hooks don't run, as for the push
itself.

The bottle never learns host paths from this: a worktree is named by its
directory's name, and only when it isn't the main one.
"""

from __future__ import annotations

import fcntl
import os
import re
import subprocess
import sys
from pathlib import Path

ZERO = "0" * 40
IN_PROGRESS = {
    "MERGE_HEAD": "a merge", "CHERRY_PICK_HEAD": "a cherry-pick", "REVERT_HEAD": "a revert",
    "BISECT_LOG": "a bisect", "rebase-merge": "a rebase", "rebase-apply": "a rebase", "index.lock": "another git command",
}

# The host's own git: none of receive-pack's environment (its namespace, its git dir, bottle's
# config for the push), and none of the repo's hooks.
ENV = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")} | {"GIT_TERMINAL_PROMPT": "0"}


def git(*args: str, cwd: Path) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-c", "core.hooksPath=/dev/null", *args], cwd=cwd, env=ENV,
                          capture_output=True, text=True)


def worktrees(git_dir: Path) -> dict[str, Path]:
    """Each checked-out branch's worktree, by branch name."""
    out = git("worktree", "list", "--porcelain", cwd=git_dir).stdout
    result, path = {}, None
    for line in out.splitlines():
        if line.startswith("worktree "):
            path = Path(line.removeprefix("worktree "))
        elif line.startswith("branch refs/heads/") and path is not None:
            result[line.removeprefix("branch refs/heads/")] = path
    return result


def busy(worktree: Path) -> str | None:
    """What git operation the worktree is in the middle of, if any."""
    for name, what in IN_PROGRESS.items():
        path = git("rev-parse", "--git-path", name, cwd=worktree).stdout.strip()
        if path and (worktree / path).exists():
            return what
    return None


def follow(branch: str, commit: str, where: Path, label: str) -> str:
    """Fast-forward `branch`, checked out at `where`, to `commit`; what happened, for the bottle."""
    head = git("rev-parse", "--verify", "--quiet", "HEAD", cwd=where).stdout.strip()
    if head == commit:
        return ""
    if git("merge-base", "--is-ancestor", head, commit, cwd=where).returncode != 0:
        return f"didn't move the user's {branch}{label}: it has commits yours doesn't"
    doing = busy(where)
    if doing:
        return f"didn't move the user's {branch}{label}: it's in the middle of {doing}"
    merged = git("merge", "--ff-only", "--quiet", commit, cwd=where)
    if merged.returncode != 0:
        lines = (merged.stderr or merged.stdout).splitlines()
        why = next((line.strip().removeprefix("error: ") for line in lines if line.strip()), f"exit {merged.returncode}")
        files = [line.strip() for line in lines if line.startswith("\t")]  # git lists what's in the way, indented
        if files:
            why += " " + ", ".join(files[:5]) + (f" and {len(files) - 5} more" if len(files) > 5 else "")
        return f"didn't move the user's {branch}{label}: {why}"
    return f"moved the user's {branch}{label} to {commit[:12]}, files and all"


def main() -> int:
    git_dir = Path(os.environ.get("GIT_DIR", ".")).resolve()
    namespace = os.environ.get("GIT_NAMESPACE", "")
    advance = os.environ.get("BOTTLE_ADVANCE", "")
    pushed = {}
    for line in sys.stdin:
        old, new, ref = line.split()
        if ref.startswith("refs/heads/") and new != ZERO:
            pushed[ref.removeprefix("refs/heads/")] = new
    if not pushed or not namespace:
        return 0

    # One push at a time moves a repo's branches, whichever bottle it's from.
    with open(git_dir / "bottle-follow.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        targets: dict[str, str] = {}  # host branch -> commit
        if advance in pushed:
            targets[advance] = pushed[advance]
        if git("config", "--bool", "bottle.follow", cwd=git_dir).stdout.strip() != "false":
            readers = _readers(git_dir, namespace)
            for remote in readers:
                git("fetch", "--quiet", remote, cwd=git_dir)
            upstreams = git("for-each-ref", "--format=%(refname:short)\t%(upstream)", "refs/heads", cwd=git_dir).stdout
            for line in upstreams.splitlines():
                local, _, upstream = line.partition("\t")
                for remote in readers:
                    branch = upstream.removeprefix(f"refs/remotes/{remote}/")
                    if branch != upstream and branch in pushed:
                        targets.setdefault(local, pushed[branch])
        checked_out = worktrees(git_dir)
        main_worktree = git_dir.parent if git_dir.name == ".git" else None
        said = []
        for branch, commit in sorted(targets.items()):
            where = checked_out.get(branch)
            if where is None:
                continue
            label = "" if where == main_worktree else f" (in worktree {where.name})"
            said.append(follow(branch, commit, where, label))
    said = [s for s in said if s]
    for line in said:
        print(f"bottle: {line}")
    log = os.environ.get("BOTTLE_HOOK_LOG")
    if log and said:
        with open(log, "a") as f:
            f.write("".join(f"{line}\n" for line in said))
    return 0


def _readers(git_dir: Path, namespace: str) -> list[str]:
    """The host's remotes that read `namespace`: their uploadpack runs upload-pack in it."""
    out = git("config", "--get-regexp", r"^remote\..*\.uploadpack$", cwd=git_dir).stdout
    readers = []
    for line in out.splitlines():
        key, _, value = line.partition(" ")
        if re.search(rf"--namespace={re.escape(namespace)}(\s|$)", value):
            readers.append(key.removeprefix("remote.").removesuffix(".uploadpack"))
    return readers


if __name__ == "__main__":
    sys.exit(main())
