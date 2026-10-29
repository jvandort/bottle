"""Opening notes -- a thread's, or the broker's -- in Obsidian, on a Mac.

Obsidian has no documented way to make a folder a vault: its URIs only open
vaults it already knows, and its CLI can't add one. So folio does what
"Open folder as vault" does itself, the undocumented way:

- **The vault's `.obsidian/`**, Obsidian's settings for it, is made if it's
  missing, so Obsidian takes the folder for a vault it has opened before
  rather than a new one (the vault's .gitignore keeps it out of git).
- **Obsidian's list of vaults**, obsidian.json, gets an entry for it:
  `{"vaults": {ID: {"path": ..., "ts": ...}}}`, an ID of 16 hex characters and
  the time in milliseconds, as Obsidian writes them. Obsidian reads the list
  when it starts, and may write it over while it runs, so a running Obsidian
  is quit first (asking, on a terminal) and opened again on the vault.

A vault Obsidian knows is opened by its ID (`obsidian://open?vault=`; `path=`
is for a file inside a vault, and its root isn't one). If obsidian.json isn't
the shape described, nothing is written, and Obsidian's vault manager is
opened instead, with the path on the clipboard for "Open folder as vault".
"""

import json
import os
import secrets
import shutil
import subprocess
import sys
import time
import urllib.parse
from pathlib import Path

QUIT_TIMEOUT = 10


def _config() -> Path:
    return Path(os.environ.get("FOLIO_OBSIDIAN_CONFIG")
                or Path.home() / "Library" / "Application Support" / "obsidian" / "obsidian.json")


def _command(name: str) -> str:
    """macOS's `open`, `pgrep` or `osascript`; FOLIO_OPEN and so on stand in for them in tests."""
    return os.environ.get(f"FOLIO_{name.upper()}", name)


def installed() -> bool:
    opener = _command("open")
    return shutil.which(opener) is not None and \
        subprocess.run([opener, "-Ra", "Obsidian"], capture_output=True).returncode == 0


def _vaults() -> dict | None:
    """obsidian.json's vaults, by ID; {} if there's no obsidian.json; None if it isn't the shape folio knows."""
    try:
        config = json.loads(_config().read_text())
    except FileNotFoundError:
        return {}
    except (OSError, ValueError):
        return None
    vaults = config.get("vaults") if isinstance(config, dict) else None
    if not isinstance(vaults, dict) or not all(isinstance(v, dict) and isinstance(v.get("path"), str)
                                               for v in vaults.values()):
        return None
    return vaults


def vault_id(path: Path) -> str | None:
    """The ID Obsidian knows the vault at `path` by; None if it doesn't know one there."""
    path = path.resolve()
    return next((vault_id for vault_id, vault in (_vaults() or {}).items() if Path(vault["path"]).resolve() == path),
                None)


def _running() -> bool:
    return subprocess.run([_command("pgrep"), "-x", "Obsidian"], capture_output=True).returncode == 0


def _quit() -> bool:
    """Quit Obsidian, as its menu would, and wait for it to go; whether it has."""
    subprocess.run([_command("osascript"), "-e", 'quit app "Obsidian"'], capture_output=True)
    deadline = time.monotonic() + QUIT_TIMEOUT
    while _running():
        if time.monotonic() > deadline:
            return False
        time.sleep(0.2)
    return True


def _register(path: Path) -> str:
    """Add the vault at `path` to obsidian.json, keeping everything else in it; its new ID."""
    config_path = _config()
    config = json.loads(config_path.read_text()) if config_path.exists() else {"vaults": {}}
    new_id = secrets.token_hex(8)
    config["vaults"][new_id] = {"path": str(path.resolve()), "ts": int(time.time() * 1000)}
    config_path.parent.mkdir(parents=True, exist_ok=True)
    staging = config_path.with_name(f".{config_path.name}.folio")
    staging.write_text(json.dumps(config))
    staging.replace(config_path)
    return new_id


def _confirm(question: str) -> bool:
    if not sys.stdin.isatty():
        return False
    try:
        return input(f"{question} [y/N] ").strip().lower() in ("y", "yes")
    except (EOFError, KeyboardInterrupt):
        print()
        return False


def open_vault(path: Path) -> str:
    """Open the vault at `path` in Obsidian, making it one Obsidian knows first; returns what to tell the user."""
    (path / ".obsidian").mkdir(exist_ok=True)
    known = vault_id(path)
    if known is None and _vaults() is not None:
        if _running():
            if not _confirm("Obsidian has to restart to add the notes as a vault. Quit and reopen it?"):
                return ("Obsidian is running, and has to restart to add the notes as a vault: quit it, and run "
                        "the command again.")
            if not _quit():
                return "Obsidian didn't quit; quit it, and run the command again."
        known = _register(path)
    if known is not None:
        subprocess.run([_command("open"), f"obsidian://open?vault={urllib.parse.quote(known)}"], check=False)
        return f"Opened {path} in Obsidian."
    copied = shutil.which("pbcopy") is not None and \
        subprocess.run(["pbcopy"], input=str(path), text=True).returncode == 0
    subprocess.run([_command("open"), "obsidian://choose-vault"], check=False)
    return (f"Obsidian's list of vaults isn't what folio expects, so it's left alone, and Obsidian's vault manager "
            f"is open: choose \"Open folder as vault\" and open\n\n    {path}\n\n"
            f"{'The path is on your clipboard: ' if copied else ''}in the folder picker, ⌘⇧G and "
            f"{'paste it' if copied else 'type it'} (~/.folio is hidden).")
