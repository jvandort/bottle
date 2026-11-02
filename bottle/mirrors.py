"""Mirrors: a remote runner's copy of each hub its bottles are served (see docs/remote-sandbox.md).

A mirror is a plain `git clone --mirror` of a hub, in the runner's
~/.bottle/git/<key>.git, its one remote, `hub`, reaching the hub on the
controller through the link: git's ext:: transport runs this machine's
relay (bottle.remote's relay), which asks the controller's bottled for
`git upload-pack` in the hub. Its refs are the hub's, exactly (fetched with
--prune), and git's own gc keeps it from growing for ever: what a bottle
borrows is as safe from gc as it is in the hub.

A bottle on the runner mounts its mirror's objects where it would mount the
hub's, and its fetches are served from the mirror once it's brought up to
date (MirrorGit), so the guest is sent little more than refs. A refresh for
an upstream fetch has the controller fetch origin into the hub first.
Pushes never touch it: they go into the hub, and come back at the next
refresh.
"""

import fcntl
import hashlib
import os
import shlex
import subprocess
from pathlib import Path

from bottle.errors import BottleError
from bottle.store import bottle_home

REMOTE = "hub"
# Set for a refresh whose fetch is upstream's: the relay asks the controller to fetch origin first.
PULL_THROUGH = "BOTTLE_PULL_THROUGH"
FETCH_TIMEOUT = 600


def key(git_dir: Path) -> str:
    """A hub's name among a runner's mirrors: its repo's name, and a hash of where it is on the controller."""
    name = git_dir.parent.name if git_dir.name == ".git" else git_dir.name.removesuffix(".git")
    return f"{name}-{hashlib.sha256(str(git_dir).encode()).hexdigest()[:8]}"


def root() -> Path:
    return bottle_home() / "git"


def path(key: str) -> Path:
    return root() / f"{key}.git"


def _relay() -> Path:
    """The command git's ext:: transport runs to reach a hub: this code's relay, through the link."""
    relay = root() / "relay"
    code = Path(__file__).resolve().parent.parent
    launcher = ("import sys; sys.path.insert(0, sys.argv.pop(1)); from bottle import remote; "
                "sys.exit(remote.main(sys.argv[1:]))")
    script = f"#!/bin/sh\nexec python3 -I -c {shlex.quote(launcher)} {shlex.quote(str(code))} relay \"$@\"\n"
    if not relay.exists() or relay.read_text() != script:
        root().mkdir(parents=True, exist_ok=True)
        partial = relay.with_suffix(".partial")
        partial.write_text(script)
        partial.chmod(0o755)
        partial.replace(relay)
    return relay


def _url(key: str) -> str:
    # ext:: splits its command at spaces; paths with them would need escaping, and bottle's have none.
    return f"ext::{_relay()} %S {key}"


def _git(*args: str, pull_through: bool = False) -> subprocess.CompletedProcess:
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")} | {"GIT_TERMINAL_PROMPT": "0"}
    if pull_through:
        env[PULL_THROUGH] = "1"
    return subprocess.run(["git", "-c", "protocol.ext.allow=always", *args], env=env, capture_output=True,
                          text=True, timeout=FETCH_TIMEOUT)


def ensure(key: str, pull_through: bool = False) -> Path:
    """The mirror of hub `key`, made if missing, brought up to date; its git dir."""
    mirror = path(key)
    root().mkdir(parents=True, exist_ok=True)
    with open(root() / f"{key}.lock", "w") as lock:
        # One at a time per mirror: a bottle's refresh and a machine's being made can overlap.
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not (mirror / "HEAD").exists():
            partial = mirror.with_suffix(".partial")
            subprocess.run(["rm", "-rf", str(partial)], check=False)
            result = _git("clone", "--quiet", "--mirror", "--origin", REMOTE, _url(key), str(partial),
                          pull_through=pull_through)
            if result.returncode != 0:
                raise BottleError(f"mirroring {key} failed: {_why(result)}")
            partial.replace(mirror)
            return mirror
        result = _git("--git-dir", str(mirror), "fetch", "--quiet", "--prune", REMOTE, pull_through=pull_through)
        if result.returncode != 0:
            raise BottleError(f"bringing the mirror of {key} up to date failed: {_why(result)}")
    return mirror


def _why(result: subprocess.CompletedProcess) -> str:
    """Why git failed: what bottle's relay said, if it said anything; else git's own fatal line."""
    lines = [line.strip() for line in result.stderr.splitlines() if line.strip()]
    for prefix in ("bottle:", "fatal:"):
        said = [line for line in lines if line.startswith(prefix)]
        if said:
            return said[0].removeprefix("bottle: ")
    return lines[-1] if lines else f"git exited {result.returncode}"
