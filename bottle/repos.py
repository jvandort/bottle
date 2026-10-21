"""Repos: what bottle needs to know about the repo a bottle is made with (`--git-repo`).

Where its objects are, to mount them; where a checkout starts; and what to
call it.
"""

import re
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError
from bottle.git import git


@dataclass(frozen=True)
class Repo:
    name: str
    path: Path


@dataclass(frozen=True)
class Start:
    """Where a bottle starts: a branch, or (branch None) a detached commit."""

    branch: str | None
    commit: str


def default_start(repo: Repo) -> Start:
    """origin's default branch; without one, whatever the checkout has checked out."""
    if not _has_ref(repo, "HEAD"):  # an unborn branch: HEAD names a branch with no commits
        raise BottleError(f"{repo.name} has no commits")
    for ref in ("refs/remotes/origin/HEAD", "HEAD"):
        try:
            branch = git("symbolic-ref", "--quiet", "--short", ref, repo=repo.path).removeprefix("origin/")
        except BottleError:
            continue  # no origin/HEAD, or a detached HEAD
        return Start(branch, resolve_branch(repo, branch))
    return Start(None, git("rev-parse", "--verify", "HEAD^{commit}", repo=repo.path))


def resolve_branch(repo: Repo, branch: str) -> str:
    """The commit `branch` points at: the local branch, else origin's."""
    for ref in (f"refs/heads/{branch}", f"refs/remotes/origin/{branch}"):
        if _has_ref(repo, ref):
            return git("rev-parse", "--verify", f"{ref}^{{commit}}", repo=repo.path)
    raise BottleError(f"{repo.name} has no branch {branch!r}")


def objects_dir(repo: Repo) -> Path:
    """Where the repo's objects live; for a linked worktree, the main repo's."""
    common = Path(git("rev-parse", "--git-common-dir", repo=repo.path))
    return (repo.path / common).resolve() / "objects"


def _has_ref(repo: Repo, ref: str) -> bool:
    try:
        git("rev-parse", "--verify", "--quiet", ref, repo=repo.path)
        return True
    except BottleError:
        return False


def derive_name(repo: Path) -> str:
    """Name a repo after its origin remote, falling back to its directory name."""
    try:
        url = git("remote", "get-url", "origin", repo=repo)
    except BottleError:
        return repo.name
    return name_from_url(url) or repo.name


def name_from_url(url: str) -> str:
    """The last path segment of a git URL, minus .git.

    Handles https://host/org/name.git, git@host:org/name.git and plain paths.
    """
    tail = re.split(r"[/:]", url.rstrip("/"))[-1]
    return tail.removesuffix(".git")


def toplevel(path: Path) -> Path:
    """The repo root, which `path` must be (not a subdirectory of it)."""
    if not path.is_dir():
        raise BottleError(f"{path} is not a directory")
    try:
        toplevel = Path(git("rev-parse", "--show-toplevel", repo=path))
    except BottleError:
        raise BottleError(f"{path} is not a git repo") from None
    # git reports the resolved path, so resolve ours (symlinks, /var -> /private/var) to compare.
    if path.resolve() != toplevel:
        raise BottleError(f"{path} is inside a git repo; give its root instead: {toplevel}")
    return toplevel
