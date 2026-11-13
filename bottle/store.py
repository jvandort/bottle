"""bottle's on-disk state under $BOTTLE_HOME (default ~/.bottle)."""

import hashlib
import json
import logging
import logging.handlers
import os
from pathlib import Path

LOG_FORMAT = "%(asctime)s %(process)d %(name)s %(message)s"


def bottle_home() -> Path:
    # An empty BOTTLE_HOME means the default, not the current directory.
    return Path(os.environ.get("BOTTLE_HOME") or Path.home() / ".bottle")


def folio_home() -> Path:
    """folio's home, $FOLIO_HOME (else ~/.folio): its threads, with their bottles' records (bottles.py), and folio
    serve's sockets and log."""
    return Path(os.environ.get("FOLIO_HOME") or Path.home() / ".folio")


def log_path(name: str = "bottle", home: Path | None = None) -> Path:
    """A log in `home`/logs ($BOTTLE_HOME's, else): "bottle" for commands; folio serve's is "serve", in folio's."""
    return (home or bottle_home()) / "logs" / f"{name}.log"


def open_log(name: str = "bottle", home: Path | None = None) -> int:
    """The log, opened for appending; only its owner can read it, or list $BOTTLE_HOME.

    The logs name every host the bottles reached, and $BOTTLE_HOME their repos.
    """
    for directory in (home or bottle_home(), log_path(name, home).parent):
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        directory.chmod(0o700)
    fd = os.open(log_path(name, home), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    os.fchmod(fd, 0o600)
    return fd


def log_to_file(name: str = "bottle", home: Path | None = None) -> None:
    """Send log records, INFO and up, to log_path(name, home), each with a timestamp. Never fails a command.

    On the root logger, so what Python and asyncio report is stamped like bottle's own.
    """
    root = logging.getLogger()
    for handler in [h for h in root.handlers if getattr(h, "bottle_log", False)]:
        root.removeHandler(handler)
        handler.close()
    try:
        handler = _Log(name, home)
    except OSError:
        return
    handler.bottle_log = True
    handler.setFormatter(logging.Formatter(LOG_FORMAT))
    root.addHandler(handler)
    root.setLevel(logging.INFO)


class _Log(logging.handlers.WatchedFileHandler):
    """A log, opened again when it's deleted or replaced (by a rotation, say), and private each time (open_log).

    folio serve runs for days: a plain FileHandler would go on writing to a log
    deleted under it, where no one can read it.
    """

    def __init__(self, name: str, home: Path | None = None) -> None:
        self.log_name, self.log_home = name, home
        super().__init__(log_path(name, home))

    def _open(self):
        return os.fdopen(open_log(self.log_name, self.log_home), "a", encoding=self.encoding, errors=self.errors)


def namespace() -> str:
    """A prefix for names that are global on a runner (machines), per BOTTLE_HOME.

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
