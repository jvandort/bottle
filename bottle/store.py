"""bottle's on-disk state under $BOTTLE_HOME (default ~/.bottle)."""

import hashlib
import json
import os
from pathlib import Path


def bottle_home() -> Path:
    # An empty BOTTLE_HOME means the default, not the current directory.
    return Path(os.environ.get("BOTTLE_HOME") or Path.home() / ".bottle")


def namespace() -> str:
    """A prefix for names that are global on the host (containers), per BOTTLE_HOME.

    Empty for the default home; otherwise a short hash, so bottles in another
    home (e.g. tests) can never collide with the default home's.
    """
    home = bottle_home().expanduser().resolve()
    if home == (Path.home() / ".bottle").resolve():
        return ""
    return hashlib.sha256(str(home).encode()).hexdigest()[:6] + "-"


def read_json(path: Path) -> dict | None:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return None


def write_json(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # Write then rename, so a crash mid-write never leaves a truncated file.
    staging = path.with_name(f".{path.name}.tmp")
    staging.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
    staging.replace(path)
