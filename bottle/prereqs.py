"""Host prerequisites, set up on first use.

Commands call the ensure_* function for what they need. Checks are silent
when everything is in place. Anything that installs software asks first.
"""

import functools
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

from bottle import krun
from bottle.errors import BottleError

# What bottle-cli is built from, in this repo: when any of it is newer than the bottle-cli built, it's built again.
SOURCES = ("Cargo.toml", "Cargo.lock", ".cargo", "agent", "agent-client", "agent-proto", "cli", "disk", "xtask")


@functools.cache
def ensure_cli() -> None:
    """bottle-cli built (see krun.py), from this repo's sources as they are now, or the one $BOTTLE_CLI names.

    Built by `cargo xtask dist` when it's missing, or older than what it's built
    from; its progress is on stderr. One named by $BOTTLE_CLI is used as it is.
    """
    if platform.system() == "Darwin" and platform.machine() != "arm64":
        raise BottleError("bottle runs on macOS on Apple silicon")
    if os.environ.get("BOTTLE_CLI"):
        if not Path(krun.binary()).is_file():
            raise BottleError(f"BOTTLE_CLI is {krun.binary()}, which isn't there")
        return
    built = Path(krun.binary())
    if built.is_file() and not _newer_than(built.stat().st_mtime):
        return
    if shutil.which("cargo") is None:
        raise BottleError("bottle builds bottle-cli with Rust's cargo; install it from https://rustup.rs")
    print("bottle: building bottle-cli (cargo xtask dist)...", file=sys.stderr)
    if subprocess.run(["cargo", "xtask", "dist"], cwd=krun.REPO, stdout=sys.stderr).returncode != 0:
        raise BottleError("`cargo xtask dist` failed")


def _newer_than(mtime: float) -> bool:
    """Whether any of bottle-cli's sources was changed after `mtime`."""
    for name in SOURCES:
        path = krun.REPO / name
        paths = [path] if path.is_file() else (p for p in path.rglob("*") if "target" not in p.parts) if path.is_dir() else []
        if any(p.is_file() and p.stat().st_mtime > mtime for p in paths):
            return True
    return False
