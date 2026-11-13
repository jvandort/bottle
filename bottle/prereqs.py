"""Host prerequisites, set up on first use.

Commands call the ensure_* function for what they need. Checks are silent
when everything is in place. Anything that installs software asks first.
"""

import fcntl
import functools
import hashlib
import os
import platform
import shlex
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
    other than these; its progress is on stderr; and a runner's, built the same way when bottle-cli copies it
    there. One named by $BOTTLE_CLI is used as it is.
    """
    if platform.system() == "Darwin" and platform.machine() != "arm64":
        raise BottleError("bottle runs on macOS on Apple silicon")
    if os.environ.get("BOTTLE_CLI"):
        if not Path(krun.binary()).is_file():
            raise BottleError(f"BOTTLE_CLI is {krun.binary()}, which isn't there")
        return
    ensure_dist()
    # And a runner's, as bottle-cli's about to copy it there (bin/bottle-dist): every bottle-cli this process runs.
    os.environ.setdefault("BOTTLE_DIST_BUILD", str(krun.REPO / "bin" / "bottle-dist"))


@functools.cache
def ensure_dist(target: str | None = None) -> Path:
    """The distribution `cargo xtask dist` builds for `target` (a Rust target triple; None: this machine's), from
    this repo's sources as they are now: built when it's missing or was built from other sources, its progress on
    stderr, one process at a time. This machine's is krun.dist(); another's is target/dist-TARGET, which on a Mac
    xtask builds in a Linux VM (xtask/src/builder.rs), with this machine's bottle-cli.
    """
    dist = krun.dist() if target is None else krun.REPO / "target" / f"dist-{target}"
    built, stamp, sources = dist / "bottle-cli", dist / STAMP, source_hash()

    def current() -> bool:
        return built.is_file() and stamp.is_file() and stamp.read_text() == sources

    if current():
        return dist
    cargo = shutil.which("cargo") or (str(RUSTUP_CARGO) if RUSTUP_CARGO.is_file() else None)
    if cargo is None:
        raise BottleError("bottle builds bottle-cli with Rust's cargo; install it from https://rustup.rs")
    dist.mkdir(parents=True, exist_ok=True)
    with open(dist.parent / f".{dist.name}.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if current():  # another built it while this waited
            return dist
        command = [cargo, "xtask", "dist", *(["--target", target] if target else []), "--out", str(dist)]
        print(f"bottle: building bottle-cli{f' for {target}' if target else ''} (cargo {shlex.join(command[1:-2])})...",
              file=sys.stderr)
        # cargo's own directory first: rustup's may not be on PATH.
        env = {**os.environ, "PATH": f"{Path(cargo).parent}{os.pathsep}{os.environ.get('PATH', '')}"}
        if subprocess.run(command, cwd=krun.REPO, stdout=sys.stderr, env=env).returncode != 0:
            raise BottleError(f"`cargo {shlex.join(command[1:-2])}` failed")
        stamp.write_text(sources)
    return dist


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
