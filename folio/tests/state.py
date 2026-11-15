"""What folio keeps, as its files have it: for tests to read what a folio command did, and to set up what one
finds, without folio's own code. (folio-cli's store.rs, repos.rs and threads.rs say what the files are.)"""

import hashlib
import json
import os
import re
import shlex
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

CURRENT = "current"
RECORD = "thread.json"
WORKSPACE_ARCHIVE, NOTES_ARCHIVE, CLAUDE_ARCHIVE, WORK = "workspace.tar.gz", "notes.tar.gz", "claude.tar.gz", "work.txt"
SNAPSHOT = (WORKSPACE_ARCHIVE, NOTES_ARCHIVE, CLAUDE_ARCHIVE, WORK)
NOTES_MOUNT = "/notes"
NOTES_REMOTES = ["host:fetch=heads", "agent:fetch=ns/notes-out,push=ns/notes-out", "user:fetch=ns/notes-in"]
GITIGNORE = ".obsidian/\n.trash/\n"
CLAUDE, SHELL = "claude", "shell"
# The picker's entry for a new thread.
NEW = "+ new thread"
DEFAULT_MEMORY = "2G"

# What each session runs, and the script that attaches to one (threads.rs).
SESSIONS = {
    CLAUDE: 'set --; '
            'if ls "$HOME/.claude/projects/$(pwd | sed "s|[/.]|-|g")"/*.jsonl >/dev/null 2>&1; '
            'then set -- --continue; fi; '
            'if command -v bottle-event >/dev/null; '
            'then set -- "$@" --dangerously-load-development-channels server:events; fi; '
            'exec claude "$@"',
    SHELL: "exec bash -l",
}
ATTACH = """\
attach() {
  for i in $(seq 20); do [ "$(stty size)" = "0 0" ] || { shpool attach -f "$1"; return; }; sleep 0.1; done
  echo "folio: the terminal has no size, so it wasn't attached" >&2; exit 1
}
started=$(date +%s)
attach "$1" && exit 0
status=$?
[ $(($(date +%s) - started)) -le 1 ] || exit $status
echo "folio: shpool couldn't show the $1 session any more, so it's been started again" >&2
shpool kill "$1"
env TERM=xterm-256color shpool attach --background --dir "$3" --cmd "$2" "$1" || exit $status
attach "$1"
"""


def session_command(session: str) -> str:
    """What a session runs, as shpool's --cmd takes it."""
    return shlex.join(["sh", "-c", SESSIONS[session]])


def workspace_out(workspace: str) -> str:
    """The script that writes a workspace's archive (snapshots.rs)."""
    return (
        f"cd {workspace} && {{ printf '.git\\0'; git ls-files -z -co --exclude-standard | "
        "xargs -0 sh -c 'for f do if [ -e \"$f\" ] || [ -L \"$f\" ]; then printf \"%s\\0\" \"$f\"; fi; done' sh; } "
        "| tar -czf - --null -T -"
    )


def folio_home() -> Path:
    return Path(os.environ.get("FOLIO_HOME") or Path.home() / ".folio")


def path_hash(path: Path) -> str:
    return hashlib.sha256(str(path).encode()).hexdigest()[:8]


def read_json(path: Path) -> dict | None:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return None


def write_json(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n")


@dataclass(frozen=True)
class Repo:
    name: str
    path: Path

    @property
    def key(self) -> str:
        return f"{self.name}-{path_hash(self.path)}"

    @property
    def directory(self) -> Path:
        return folio_home() / self.key

    def spec(self) -> dict | None:
        """The repo's spec, as folio reads it: one from before memory was given gets the default, and every one has
        the features folio needs."""
        spec = read_json(self.directory / "spec.json")
        if spec is None:
            return None
        if spec.get("memory", "all") == "all":
            spec = {**spec, "memory": DEFAULT_MEMORY}
        features = list(spec.get("features", []))
        if "tools" not in {f.split(":", 1)[0] for f in features}:
            features.append("tools")
        return {**spec, "features": features}

    def save_spec(self, spec: dict) -> None:
        if not (self.directory / "repo.json").exists():
            write_json(self.directory / "repo.json", {"name": self.name, "path": str(self.path)})
        write_json(self.directory / "spec.json", spec)


def repo(cwd: Path) -> Repo:
    """The repo `cwd` is in, as folio names and keys it: its origin's name, else its directory's."""
    toplevel = Path(subprocess.run(["git", "-C", str(cwd), "rev-parse", "--show-toplevel"], capture_output=True,
                                   text=True, check=True).stdout.strip())
    url = subprocess.run(["git", "-C", str(toplevel), "remote", "get-url", "origin"], capture_output=True,
                         text=True).stdout.strip()
    name = re.split(r"[/:]", url.rstrip("/"))[-1].removesuffix(".git") if url else ""
    name = re.sub(r"[^A-Za-z0-9._-]+", "-", name or toplevel.name).strip("-._")
    return Repo(name or "repo", toplevel)


def known() -> list[Repo]:
    home = folio_home()
    found = []
    for entry in sorted(home.glob("*/repo.json")) if home.is_dir() else ():
        data = read_json(entry) or {}
        if "name" in data and "path" in data:
            found.append(Repo(data["name"], Path(data["path"])))
    return found


def current_slot(directory: Path) -> Path | None:
    """The slot `current` points at, or None if nothing has been written."""
    link = directory / CURRENT
    return directory / os.readlink(link) if link.is_symlink() else None


def write_slot(directory: Path, files: dict[str, bytes | None]) -> Path:
    """A new slot in `directory`, as folio writes one: `files` (None: left out), and every other file of the
    current slot, linked; made current."""
    directory.mkdir(parents=True, exist_ok=True)
    old = current_slot(directory)
    while True:
        now = time.time()
        slot = directory / (time.strftime("%Y%m%dT%H%M%S", time.gmtime(now)) + f".{int(now * 1000) % 1000:03d}Z")
        try:
            slot.mkdir()
            break
        except FileExistsError:
            time.sleep(0.001)
    for name, content in files.items():
        if content is not None:
            (slot / name).write_bytes(content)
    if old is not None:
        for kept in old.iterdir():
            if kept.name not in files:
                os.link(kept, slot / kept.name)
    staging = directory / f".{CURRENT}.test"
    staging.unlink(missing_ok=True)
    staging.symlink_to(slot.name)
    staging.replace(directory / CURRENT)
    if old is not None:
        import shutil
        shutil.rmtree(old, ignore_errors=True)
    return slot


def thread(repo_: Repo, name: str) -> dict:
    """A thread's record."""
    return json.loads((current_slot(repo_.directory / name) / RECORD).read_text())


def broker_repo() -> Repo:
    """The broker's identity, for where folio keeps its state: the path its vault used to be."""
    return Repo("broker", folio_home() / "repos" / "broker")


def broker_notes() -> Path:
    return broker_repo().directory / "broker" / "notes"
