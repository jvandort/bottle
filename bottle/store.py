"""bottle's on-disk state under $BOTTLE_HOME (default ~/.bottle)."""

import hashlib
import json
import logging
import os
from pathlib import Path

LOG_FORMAT = "%(asctime)s %(process)d %(name)s %(message)s"


def bottle_home() -> Path:
    # An empty BOTTLE_HOME means the default, not the current directory.
    return Path(os.environ.get("BOTTLE_HOME") or Path.home() / ".bottle")


def log_path(name: str = "bottle") -> Path:
    """A log in $BOTTLE_HOME/logs: "bottle" for commands, "bottled" for the daemon."""
    return bottle_home() / "logs" / f"{name}.log"


def open_log(name: str = "bottle") -> int:
    """The log, opened for appending; only its owner can read it, or list $BOTTLE_HOME.

    The logs name every host the bottles reached, and $BOTTLE_HOME their repos.
    """
    for directory in (bottle_home(), log_path(name).parent):
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        directory.chmod(0o700)
    fd = os.open(log_path(name), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    os.fchmod(fd, 0o600)
    return fd


def log_to_file(name: str = "bottle") -> None:
    """Send log records, INFO and up, to log_path(name), each with a timestamp. Never fails a command.

    On the root logger, so what Python and asyncio report is stamped like bottle's own.
    """
    root = logging.getLogger()
    for handler in [h for h in root.handlers if getattr(h, "bottle_log", False)]:
        root.removeHandler(handler)
        handler.close()
    try:
        os.close(open_log(name))  # creates it private; the handler then opens the file itself
        handler = logging.FileHandler(log_path(name))
    except OSError:
        return
    handler.bottle_log = True
    handler.setFormatter(logging.Formatter(LOG_FORMAT))
    root.addHandler(handler)
    root.setLevel(logging.INFO)


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
