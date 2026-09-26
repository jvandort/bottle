"""Wrapped repos: the user's local git repos that bottles can be created from.

The registry at $BOTTLE_HOME/repos.json maps each repo's name to its location.
Bottles later mount the repo's objects read-only; nothing is copied here.
"""

import json
import os
import re
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError
from bottle.git import git

NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


def bottle_home() -> Path:
    return Path(os.environ.get("BOTTLE_HOME", Path.home() / ".bottle"))


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
    try:
        data = json.loads(registry_path().read_text())
    except FileNotFoundError:
        return {}
    return {name: Repo(name, Path(entry["path"])) for name, entry in data["repos"].items()}


def _save(repos: dict[str, Repo]) -> None:
    data = {"version": 1, "repos": {r.name: {"path": str(r.path)} for r in repos.values()}}
    path = registry_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    # Write then rename, so a crash mid-write never leaves a truncated registry.
    staging = path.with_name(f".{path.name}.tmp")
    staging.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
    staging.replace(path)


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
