"""bottle's on-disk state under $BOTTLE_HOME (default ~/.bottle)."""

import json
import os
from pathlib import Path


def bottle_home() -> Path:
    return Path(os.environ.get("BOTTLE_HOME", Path.home() / ".bottle"))


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
