"""Bottles as smolvm machines. All `smolvm` invocations live here.

smolvm (github.com/smol-machines/smolvm) runs each machine as a libkrun VM
with its own kernel, booting its own small Linux whose agent runs our image
as a container. Bottle relies on, and tests/test_smolvm_contract.py checks:

  - mediated egress: started with --egress-interceptor and --mediated-egress,
    a machine asks bottled's decider about each TCP flow before connecting it (see
    decider.py), and denies other UDP than DNS. Needs --net-backend virtio-net.
  - DNS, which smolvm answers itself, from the host's resolver as it was when
    the machine started, so the Mac's resolver mustn't change under it.
  - exec, always with -i: a plain exec collects output in the guest and cuts
    it at 11 MiB with nothing on stderr, and snapshots go through exec.
  - read-only mounts, -e reaching every exec, and a disk kept across stops.

Machines, and running commands in them, are all this module does: building
the images machines boot is images.py's, on top of these.
"""

import json
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError


def binary() -> str:
    """The smolvm bottle runs: $BOTTLE_SMOLVM if set, else `smolvm` on PATH."""
    return os.environ.get("BOTTLE_SMOLVM") or "smolvm"


# A machine's caps. vCPUs cost nothing idle; memory is committed as the guest touches it.
MAX_CPUS = 255
# The most guest TCP connections a machine holds at once, counting those still
# closing: smolvm frees a slot only once its side is Closed, so one the far end
# closed first holds it through smoltcp's 10s TIME_WAIT. smolvm's default, 512,
# a Gradle build's downloads outran: past it, a new connection is refused at
# its SYN ("relay table is full" in the machine's agent-startup-error.log).
# 4096 is the most smolvm takes.
TCP_RELAY_CAPACITY = 4096



# --- machines -------------------------------------------------------------------


@dataclass(frozen=True)
class Mount:
    source: Path
    target: str


def create(name: str, image: str, mounts: list[Mount], env: dict[str, str], memory_mib: int | None, cpus: int) -> None:
    """Create a stopped machine from `image` (an archive path), its mounts read-only; `memory_mib` None: smolvm's default."""
    args = ["machine", "create", "--name", name, "--image", image, "--net", "--net-backend", "virtio-net",
            "--cpus", str(min(cpus, MAX_CPUS)), *(["--mem", str(memory_mib)] if memory_mib else [])]
    for m in mounts:
        args += ["-v", f"{m.source}:{m.target}:ro"]
    for key, value in env.items():
        args += ["-e", f"{key}={value}"]
    _run(*args)


def start(name: str, decider: str, token: str) -> None:
    """Start a machine, asking `decider` about each flow and presenting `token`.

    smolvm keeps both for this run of the machine only, so every start passes them.
    """
    _run("machine", "start", "--name", name, "--egress-interceptor", decider, "--mediated-egress",
         env={"SMOLVM_INTERCEPTOR_TOKEN": token, "SMOLVM_TCP_RELAY_CAPACITY": str(TCP_RELAY_CAPACITY)})


def stop(name: str) -> None:
    _run("machine", "stop", "--name", name)


def delete(name: str) -> None:
    """Delete a machine; a no-op if there's none."""
    if state(name) is not None:
        _run("machine", "delete", "--name", name, "-f")


def state(name: str) -> str | None:
    """"running", "stopped", "created", ..., or None if there's no such machine."""
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
    return [binary(), *_GLOBAL, *exec_args(name, argv, user, workdir, env, tty)]


def exec_args(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              env: dict[str, str] | None = None, tty: bool = False) -> list[str]:
    # Always -i: without it, output past 11 MiB is cut silently.
    args = ["machine", "exec", "--name", name, "-i", *(["-t"] if tty else [])]
    if user:
        args += ["-u", user]
    if workdir:
        args += ["-w", workdir]
    for key, value in (env or {}).items():
        args += ["-e", f"{key}={value}"]
    return [*args, "--", *argv]


# --- the CLI --------------------------------------------------------------------


# smolvm's own failures as JSON on stderr, so they read as smolvm's.
_GLOBAL = ["--error-format", "json"]


class Missing(BottleError):
    """smolvm found no such machine."""


def _run(*args: str, input: str | None = None, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(
        [binary(), *_GLOBAL, *args], capture_output=True, text=True, input=input,
        env={**os.environ, **env} if env else None,
    )
    if result.returncode != 0:
        code, message = _error(result.stderr)
        # `machine status` reports a machine that isn't there as a config error, not NOT_FOUND
        # (smolvm 1.31.2: "config operation failed: machine status: machine 'x' not found").
        if code == "NOT_FOUND" or (code == "CONFIG_ERROR" and message.endswith("not found")):
            raise Missing(message)
        raise BottleError(message or f"smolvm {' '.join(args[:2])} failed (exit {result.returncode})")
    return result.stdout


def _error(stderr: str) -> tuple[str | None, str]:
    """smolvm's error code and message, from its JSON on stderr; else no code and stderr as it is."""
    for line in reversed(stderr.strip().splitlines()):
        try:
            error = json.loads(line)["error"]
            return error.get("code"), error.get("message", "")
        except (ValueError, KeyError, TypeError):
            continue
    return None, stderr.strip()
