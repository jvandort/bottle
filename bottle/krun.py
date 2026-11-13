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

import json
import os
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError
from bottle.store import bottle_home

REPO = Path(__file__).resolve().parent.parent


def binary() -> str:
    """The bottle-cli bottle runs: $BOTTLE_CLI if set, else the one `cargo xtask dist` built (see dist)."""
    return os.environ.get("BOTTLE_CLI") or str(dist() / "bottle-cli")


def dist() -> Path:
    """Where `cargo xtask dist` builds bottle-cli, from this repo: its target/dist."""
    return REPO / "target" / "dist"


# --- machines -------------------------------------------------------------------


@dataclass(frozen=True)
class Mount:
    source: Path
    target: str


def create(name: str, image: str, mounts: list[Mount], env: dict[str, str], memory_mib: int | None, cpus: int,
           disk_gib: int | None = None, user: str | None = None, workdir: str | None = None,
           labels: dict[str, str] | None = None, network: dict | None = None, cli: "Cli | None" = None) -> None:
    """Create a stopped machine from `image` (a stage's disk), its mounts read-only; `memory_mib` and `disk_gib`
    None: bottle-cli's defaults. The disk is its own, sparse, where what the machine writes goes. `user` and
    `workdir` are who and where its commands run unless they say; `labels`, what `bottle-cli ls` shows of it;
    `network`, the policy every connection it makes is decided by (services.network), kept with it."""
    args = ["machine", "create", "--name", name, "--image", image, "--cpus", str(cpus),
            *(["--mem", str(memory_mib)] if memory_mib else []), *(["--storage", str(disk_gib)] if disk_gib else []),
            *(["--user", user] if user else []), *(["--workdir", workdir] if workdir else [])]
    for key, value in (labels or {}).items():
        args += ["--label", f"{key}={value}"]
    for m in mounts:
        args += ["-v", f"{m.source}:{m.target}:ro"]
    for key, value in env.items():
        args += ["-e", f"{key}={value}"]
    if network is not None:
        args += ["--network", "-"]
    _run(*args, input=json.dumps(network) if network is not None else None, cli=cli)


def configure(name: str, network: dict, cli: "Cli | None" = None) -> None:
    """Give a stopped machine the policy its connections are decided by (one made before it had one, say)."""
    _run("machine", "configure", "--name", name, "--network", "-", input=json.dumps(network), cli=cli)


def start(name: str, cli: "Cli | None" = None) -> None:
    """Start a machine: by the policy it was made with, nothing else needed."""
    _run("machine", "start", "--name", name, cli=cli)


def ca_certificate(name: str, hosts: list[str], cli: "Cli | None" = None) -> str | None:
    """The certificate of the caching CA bottle-cli answers `name`'s cached https `hosts` behind (a machine's, or
    images.BUILDS_CA for builds), made if missing; None for no hosts."""
    return _run("ca", "certificate", name, *hosts, cli=cli) or None


def stop(name: str, cli: "Cli | None" = None) -> None:
    _run("machine", "stop", "--name", name, cli=cli)


def rename(name: str, new: str, cli: "Cli | None" = None) -> None:
    """Rename a stopped machine."""
    _run("machine", "rename", "--name", name, "--to", new, cli=cli)


def delete(name: str, cli: "Cli | None" = None) -> None:
    """Delete a machine; a no-op if there's none."""
    if state(name, cli=cli) is not None:
        _run("machine", "delete", "--name", name, "-f", cli=cli)


def running(cli: "Cli | None" = None) -> list[str]:
    """The machines running there, whoever made them: any that isn't stopped (or never started)."""
    machines = json.loads(_run("machine", "ls", "--json", cli=cli) or "[]")
    return sorted(m["name"] for m in machines if m.get("state") not in ("stopped", "created"))


def state(name: str, cli: "Cli | None" = None) -> str | None:
    """"running", "stopped", "created", or None if there's no such machine."""
    try:
        return json.loads(_run("machine", "status", "--name", name, "--json", cli=cli))["state"]
    except Missing:
        return None


def exec_(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
          input: str | None = None, env: dict[str, str] | None = None, cli: "Cli | None" = None) -> str:
    """Run a command in a running machine and return its output; `input` is its stdin."""
    return _run(*exec_args(name, argv, user, workdir, env), input=input if input is not None else "", cli=cli)


def exec_command(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                 tty: bool = False, env: dict[str, str] | None = None, cli: "Cli | None" = None) -> list[str]:
    """The command line that runs `argv` in a machine with stdin attached (and a TTY), for other tools to run."""
    return (cli or LOCAL).command(exec_args(name, argv, user, workdir, env, tty), tty=tty)


def info(cli: "Cli | None" = None) -> dict:
    """The machine's memory (memory_mib) and cores (cpus), as bottle-cli there sees them."""
    return json.loads(_run("info", "--json", cli=cli))


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


class Cli:
    """Where bottle-cli runs: on this machine (LOCAL), or a runner's (RunnerCli)."""

    def command(self, args: list[str], tty: bool | None = None) -> list[str]:
        """The command line that runs bottle-cli with `args` there."""
        return [binary(), *args]

    def home(self) -> str:
        """$BOTTLE_HOME there, absolute."""
        return str(bottle_home())

    def run(self, args: list[str], input: str | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(self.command(args, tty=False), capture_output=True, text=True, input=input)

    def build(self, recipe: dict, network: dict, context: Path, store: str, tag: str) -> subprocess.CompletedProcess:
        """`bottle-cli image build` of `recipe`, mounting `context`, by `network`'s policy, tagged `tag` in `store`:
        its progress on this process's stderr, the disks it made on stdout."""
        with tempfile.TemporaryDirectory(prefix="build-") as here:
            made = {**recipe, "mount": {**recipe["mount"], "source": str(context.resolve())}}
            (Path(here) / "recipe.json").write_text(json.dumps(made))
            (Path(here) / "network.json").write_text(json.dumps(network))
            return subprocess.run(self.command(["image", "build", f"{here}/recipe.json", store, "--tag", tag,
                                                "--network", f"{here}/network.json"]),
                                  stdout=subprocess.PIPE, text=True)


LOCAL = Cli()


class RunnerCli(Cli):
    """bottle-cli on runner `runner`, as this one runs it there (`bottle-cli on RUNNER`): what images builds
    and keeps there. An image build sends its recipe, what it mounts and its policy with it."""

    def __init__(self, runner: str) -> None:
        self.runner = runner
        self._home: str | None = None

    def command(self, args: list[str], tty: bool | None = None) -> list[str]:
        return [binary(), "on", self.runner, *args]

    def home(self) -> str:
        if self._home is None:
            self._home = info(cli=self)["home"]
        return self._home


def _run(*args: str, input: str | None = None, cli: Cli | None = None) -> str:
    result = (cli or LOCAL).run(list(args), input=input)
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
