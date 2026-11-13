"""Bottles as bottle-cli's machines. All `bottle-cli machine` invocations live here.

bottle-cli (cli/ in this repo, built by `cargo xtask dist`) runs each machine
as a libkrun VM of its own, booting the image's stage disks stacked by
overlayfs, with bottle's agent as its init. Bottle relies on, and
tests/test_krun_contract.py checks:

  - decided egress: a machine decides each TCP connection by the policy it
    was made with (services.network), caching what it says, and gives what it
    routes to bottled's services socket (see services.py); it has no UDP.
  - DNS, answered in the guest from the runner's resolver at the time of each
    lookup.
  - exec, its stdin, stdout and stderr streamed, and exit status kept.
  - read-only mounts, the image's environment and -e reaching every exec, and a
    disk kept across stops.

Machines, and running commands in them, are all this module does: building
the images machines boot is images.py's, on top of bottle-cli's builds.
"""

import contextlib
import json
import os
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError

REPO = Path(__file__).resolve().parent.parent


def binary() -> str:
    """The bottle-cli bottle runs: $BOTTLE_CLI if set, else the one `cargo xtask dist` built (see dist)."""
    return os.environ.get("BOTTLE_CLI") or str(dist() / "bottle-cli")


def dist() -> Path:
    """Where `cargo xtask dist` builds bottle-cli, from this repo: $BOTTLE_DIST (a runner's, see remote.py), else
    the repo's target/dist."""
    return Path(os.environ.get("BOTTLE_DIST") or REPO / "target" / "dist")


# A machine's caps. vCPUs cost nothing idle; memory is committed as the guest touches it.
MAX_CPUS = 16


# --- machines -------------------------------------------------------------------


@dataclass(frozen=True)
class Mount:
    source: Path
    target: str


def create(name: str, image: str, mounts: list[Mount], env: dict[str, str], memory_mib: int | None, cpus: int,
           disk_gib: int | None = None, user: str | None = None, workdir: str | None = None,
           labels: dict[str, str] | None = None, network: dict | None = None) -> None:
    """Create a stopped machine from `image` (a stage's disk), its mounts read-only; `memory_mib` and `disk_gib`
    None: bottle-cli's defaults. The disk is its own, sparse, where what the machine writes goes. `user` and
    `workdir` are who and where its commands run unless they say; `labels`, what `bottle-cli ls` shows of it;
    `network`, the policy every connection it makes is decided by (services.network), kept with it."""
    args = ["machine", "create", "--name", name, "--image", image, "--cpus", str(min(cpus, MAX_CPUS)),
            *(["--mem", str(memory_mib)] if memory_mib else []), *(["--storage", str(disk_gib)] if disk_gib else []),
            *(["--user", user] if user else []), *(["--workdir", workdir] if workdir else [])]
    for key, value in (labels or {}).items():
        args += ["--label", f"{key}={value}"]
    for m in mounts:
        args += ["-v", f"{m.source}:{m.target}:ro"]
    for key, value in env.items():
        args += ["-e", f"{key}={value}"]
    with _policy(network) as path:
        _run(*args, *(["--network", path] if path else []))


def configure(name: str, network: dict) -> None:
    """Give a stopped machine the policy its connections are decided by (one made before it had one, say)."""
    with _policy(network) as path:
        _run("machine", "configure", "--name", name, "--network", path)


@contextlib.contextmanager
def _policy(network: dict | None):
    """A network policy as a file, for bottle-cli to take, while it's needed; None for none."""
    if network is None:
        yield None
        return
    with tempfile.NamedTemporaryFile("w", suffix=".json", prefix="network-") as file:
        json.dump(network, file)
        file.flush()
        yield file.name


def start(name: str) -> None:
    """Start a machine: by the policy it was made with, nothing else needed."""
    _run("machine", "start", "--name", name)


def ca_certificate(name: str, hosts: list[str]) -> str | None:
    """The certificate of the caching CA bottle-cli answers `name`'s cached https `hosts` behind (a machine's, or
    images.BUILDS_CA for builds), made if missing; None for no hosts."""
    return _run("ca", "certificate", name, *hosts) or None


def stop(name: str) -> None:
    _run("machine", "stop", "--name", name)


def rename(name: str, new: str) -> None:
    """Rename a stopped machine."""
    _run("machine", "rename", "--name", name, "--to", new)


def delete(name: str) -> None:
    """Delete a machine; a no-op if there's none."""
    if state(name) is not None:
        _run("machine", "delete", "--name", name, "-f")


def running() -> list[str]:
    """The machines running here, whoever made them: any that isn't stopped (or never started)."""
    machines = json.loads(_run("machine", "ls", "--json") or "[]")
    return sorted(m["name"] for m in machines if m.get("state") not in ("stopped", "created"))


def state(name: str) -> str | None:
    """"running", "stopped", "created", or None if there's no such machine."""
    try:
        return json.loads(_run("machine", "status", "--name", name, "--json"))["state"]
    except Missing:
        return None


def exec_(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
          input: str | None = None, env: dict[str, str] | None = None) -> str:
    """Run a command in a running machine and return its output; `input` is its stdin."""
    return _run(*exec_args(name, argv, user, workdir, env), input=input if input is not None else "")


def exec_command(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                 tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
    """The command line that runs `argv` in a machine with stdin attached (and a TTY), for other tools to run."""
    return [binary(), *exec_args(name, argv, user, workdir, env, tty)]


def exec_args(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              env: dict[str, str] | None = None, tty: bool = False) -> list[str]:
    args = ["machine", "exec", "--name", name, *(["-t"] if tty else [])]
    if user:
        args += ["-u", user]
    if workdir:
        args += ["-w", workdir]
    for key, value in (env or {}).items():
        args += ["-e", f"{key}={value}"]
    return [*args, "--", *argv]


# --- the CLI --------------------------------------------------------------------


class Missing(BottleError):
    """bottle-cli found no such machine."""


def _run(*args: str, input: str | None = None, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(
        [binary(), *args], capture_output=True, text=True, input=input,
        env={**os.environ, **env} if env else None,
    )
    if result.returncode != 0:
        message = _error(result.stderr)
        if message.startswith("no machine "):
            raise Missing(message)
        raise BottleError(message or f"bottle-cli {' '.join(args[:2])} failed (exit {result.returncode})")
    return result.stdout


def _error(stderr: str) -> str:
    """bottle-cli's message, from the last line it wrote on stderr ("bottle-cli: ..."), else stderr as it is."""
    lines = stderr.strip().splitlines()
    for line in reversed(lines):
        if line.startswith("bottle-cli: "):
            return line.removeprefix("bottle-cli: ")
    return stderr.strip()
