"""Host prerequisites, set up on first use.

Commands call the ensure_* function for what they need. Checks are silent
when everything is in place. Anything that installs software asks first.
"""

import fcntl
import functools
import hashlib
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

from bottle import krun
from bottle.errors import BottleError

# What bottle-cli is built from, in this repo: when it's changed since bottle-cli was built, it's built again.
SOURCES = ("Cargo.toml", "Cargo.lock", ".cargo", "agent", "agent-client", "agent-proto", "cli", "disk", "xtask")
# Beside bottle-cli: the hash of the sources it was built from.
STAMP = ".sources"


@functools.cache
def ensure_cli() -> None:
    """bottle-cli built (see krun.py), from this repo's sources as they are now, or the one $BOTTLE_CLI names.

    Built by `cargo xtask dist` when it's missing, or was built from sources
    other than these; its progress is on stderr. One process builds at a time.
    One named by $BOTTLE_CLI is used as it is.
    """
    if platform.system() == "Darwin" and platform.machine() != "arm64":
        raise BottleError("bottle runs on macOS on Apple silicon")
    if os.environ.get("BOTTLE_CLI"):
        if not Path(krun.binary()).is_file():
            raise BottleError(f"BOTTLE_CLI is {krun.binary()}, which isn't there")
        return
    dist = krun.dist()
    built, stamp, sources = dist / "bottle-cli", dist / STAMP, source_hash()

    def current() -> bool:
        return built.is_file() and stamp.is_file() and stamp.read_text() == sources

    if current():
        return
    cargo = shutil.which("cargo") or (str(RUSTUP_CARGO) if RUSTUP_CARGO.is_file() else None)
    if cargo is None:
        raise BottleError("bottle builds bottle-cli with Rust's cargo; install it from https://rustup.rs")
    dist.mkdir(parents=True, exist_ok=True)
    with open(dist / ".lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if current():  # another built it while this waited
            return
        print("bottle: building bottle-cli (cargo xtask dist)...", file=sys.stderr)
        # cargo's own directory first: a non-interactive ssh shell's PATH may not have rustup's.
        env = {**os.environ, "PATH": f"{Path(cargo).parent}{os.pathsep}{os.environ.get('PATH', '')}"}
        if subprocess.run([cargo, "xtask", "dist", "--out", str(dist)], cwd=krun.REPO, stdout=sys.stderr,
                          env=env).returncode != 0:
            raise BottleError("`cargo xtask dist` failed")
        stamp.write_text(sources)


# Where rustup puts cargo, for when it isn't on PATH.
RUSTUP_CARGO = Path.home() / ".cargo" / "bin" / "cargo"


def source_files() -> list[Path]:
    """bottle-cli's sources in this repo: SOURCES, less what cargo builds in a target directory."""
    found = []
    for name in SOURCES:
        path = krun.REPO / name
        paths = [path] if path.is_file() else path.rglob("*") if path.is_dir() else []
        found += [p for p in paths if p.is_file() and "target" not in p.relative_to(krun.REPO).parts]
    return sorted(found)


def source_hash() -> str:
    """A hash of bottle-cli's sources: their names and contents."""
    digest = hashlib.sha256()
    for path in source_files():
        digest.update(str(path.relative_to(krun.REPO)).encode() + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()
