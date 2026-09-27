"""Wrapped repos: the user's local git repos that bottles can be created from.

The registry at $BOTTLE_HOME/repos.json maps each repo's name to its location.
Bottles later mount the repo's objects read-only; nothing is copied here.
"""

import re
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError
from bottle.git import git
from bottle.store import bottle_home, read_json, write_json

NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


def registry_path() -> Path:
    return bottle_home() / "repos.json"


@dataclass(frozen=True)
class Repo:
    name: str
    path: Path


@dataclass(frozen=True)
class WrapResult:
    repo: Repo
    created: bool


def wrap(path: Path, name: str | None = None) -> WrapResult:
    """Register the repo at `path` under `name`. Re-wrapping the same repo is a no-op."""
    source = _toplevel(path)
    name = name or derive_name(source)
    if not NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid name {name!r}: use letters, digits, '.', '_' or '-'")

    repos = load()
    if name in repos:
        if repos[name].path != source:
            raise BottleError(f"{name!r} already wraps {repos[name].path}; choose another name")
        return WrapResult(repos[name], created=False)
    for other in repos.values():
        if other.path == source:
            raise BottleError(f"{source} is already wrapped as {other.name!r}")

    repo = Repo(name, source)
    repos[name] = repo
    _save(repos)
    return WrapResult(repo, created=True)


def load() -> dict[str, Repo]:
    data = read_json(registry_path()) or {"repos": {}}
    return {name: Repo(name, Path(entry["path"])) for name, entry in data["repos"].items()}


def get(name: str) -> Repo:
    repos = load()
    if name not in repos:
        known = ", ".join(sorted(repos)) or "none; add one with `bottle wrap`"
        raise BottleError(f"no wrapped repo named {name!r} (wrapped: {known})")
    return repos[name]


def _save(repos: dict[str, Repo]) -> None:
    write_json(registry_path(), {"version": 1, "repos": {r.name: {"path": str(r.path)} for r in repos.values()}})


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


def _toplevel(path: Path) -> Path:
    """The repo root, which `path` must be (not a subdirectory of it)."""
    if not path.is_dir():
        raise BottleError(f"{path} is not a directory")
    try:
        toplevel = Path(git("rev-parse", "--show-toplevel", repo=path))
    except BottleError:
        raise BottleError(f"{path} is not a git repo") from None
    # git reports the resolved path, so resolve ours (symlinks, /var -> /private/var) to compare.
    if path.resolve() != toplevel:
        raise BottleError(f"{path} is inside a git repo; wrap its root instead: {toplevel}")
    return toplevel
