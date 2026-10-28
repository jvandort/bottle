"""Repos: what folio knows about each repo it has been used in.

There's no registering a repo: the first folio command in one gives it a
directory, ~/.folio/<key>/. The key is the repo's name and a hash of its path,
`<name>-<hash>`: readable, and two repos with the same name never collide.
The path is the toplevel as `git rev-parse --show-toplevel` gives it, which on
macOS resolves symlinks and has the casing that's on disk, so however the
repo's directory is typed, the key is the same. Moving a repo orphans what
folio knows about it; that's for later.
"""

import hashlib
import re
from dataclasses import dataclass
from pathlib import Path

from . import store
from .errors import FolioError
from .git import git

# What a new repo's bottles get until `folio configure` says otherwise: tools
# has shpool, which every thread runs in; and memory, which every spec gives.
DEFAULT_MEMORY = "2G"
DEFAULT_SPEC = {"features": ["tools"], "memory": DEFAULT_MEMORY}
# Under folio's home, the repos folio owns (owned_path).
OWNED = "repos"
# What a repo's spec can't say: a thread's bottle never has permissions, and
# folio names its thread itself.
NOT_IN_A_REPOS_SPEC = ("permissions", "thread")


def check_repo_spec(spec: dict) -> dict:
    """`spec`, if it's one a repo's threads can be made from."""
    given = [key for key in NOT_IN_A_REPOS_SPEC if key in spec]
    if given:
        raise FolioError(f"a repo's spec can't give {' or '.join(given)}: its threads' bottles never have permissions, "
                         "and each one's thread is its own")
    return spec


@dataclass(frozen=True)
class Repo:
    name: str
    path: Path

    @property
    def key(self) -> str:
        return f"{self.name}-{path_hash(self.path)}"

    @property
    def directory(self) -> Path:
        return store.folio_home() / self.key

    @property
    def spec_path(self) -> Path:
        return self.directory / "spec.json"

    def spec(self) -> dict | None:
        """The repo's bottle spec, as last saved; None until it has one.

        One saved before every spec had to give its memory (none, or "all")
        gets the default.
        """
        spec = store.read_json(self.spec_path)
        if spec is not None and spec.get("memory", "all") == "all":
            spec = {**spec, "memory": DEFAULT_MEMORY}
        return spec

    def save_spec(self, spec: dict) -> None:
        self.remember()
        store.write_json(self.spec_path, spec)

    def remember(self) -> None:
        """Give the repo its directory, if it hasn't one yet."""
        if not (self.directory / "repo.json").exists():
            store.write_json(self.directory / "repo.json", {"name": self.name, "path": str(self.path)})


def path_hash(path: Path) -> str:
    return hashlib.sha256(str(path).encode()).hexdigest()[:8]


def owned_path(name: str) -> Path:
    """Where folio keeps a repo it owns, rather than one of yours: the broker's vault."""
    return store.folio_home() / OWNED / name


def owned(toplevel: Path) -> bool:
    root = store.folio_home() / OWNED
    return toplevel.parent in (root, root.resolve())


def current(cwd: Path | None = None) -> Repo:
    """The repo the working directory is in."""
    try:
        toplevel = Path(git("rev-parse", "--show-toplevel", repo=cwd))
    except FolioError:
        raise FolioError("not in a git repo; folio works on the repo you're in") from None
    if owned(toplevel):
        raise FolioError(f"{toplevel} is folio's own; `folio {toplevel.name}` works in it")
    return Repo(_name(toplevel), toplevel)


def known() -> list[Repo]:
    """Every repo folio has a directory for, wherever it's run."""
    home = store.folio_home()
    repos = []
    for entry in sorted(home.glob("*/repo.json")) if home.is_dir() else ():
        data = store.read_json(entry) or {}
        if "name" in data and "path" in data:
            repos.append(Repo(data["name"], Path(data["path"])))
    return repos


def _name(toplevel: Path) -> str:
    """The origin remote's repo name, else the directory's, as something a bottle may be named after."""
    try:
        url = git("remote", "get-url", "origin", repo=toplevel)
        name = re.split(r"[/:]", url.rstrip("/"))[-1].removesuffix(".git")
    except FolioError:
        name = ""
    name = re.sub(r"[^A-Za-z0-9._-]+", "-", name or toplevel.name).strip("-._")
    return name or "repo"


def merge_features(current: list[str], given: list[str]) -> list[str]:
    """`current` with `given` added: a feature given again replaces its options."""
    merged = {spec.split(":", 1)[0]: spec for spec in current}
    for spec in given:
        merged[spec.split(":", 1)[0]] = spec
    return list(merged.values())
