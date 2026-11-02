"""Runners: the machines bottles run on, behind one interface (see docs/remote-sandbox.md).

A runner holds bottles' machines, the images they boot, and the decider their
connections are asked about. Everything else -- the registry, credentials,
the repos bottles are served -- is the driver's, the machine bottle's commands
run on. The code on the driver's side reaches machines only through a
Runner, so it can't tell which runner it has.

The local runner is the driver itself: today's smolvm.py, images.py and
bottled, called in-process. Its decider is bottled's.

Code on the runner's side (smolvm.py, images.py's store and builds, the
decider) uses its neighbours directly: a builder machine is the runner's own
business. Building the images a bottle boots still starts on the driver, in
features.ensure_built, and goes through the runner once runners can be remote.
"""

from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from bottle import daemon, images, prereqs, repos, smolvm
from bottle.errors import BottleError
from bottle.host import Served

LOCAL = "local"


@dataclass(frozen=True)
class Mount:
    """A directory of the runner's, mounted read-only into a machine at `target`."""

    source: Path
    target: str


class Runner(Protocol):
    name: str

    def ensure_ready(self) -> None:
        """Everything the runner needs to run machines, set up (asking first) or refused."""

    def archive(self, tag: str) -> Path | None:
        """The archive `tag` was last built into, in the runner's image store; None if it isn't built there."""

    def objects(self, repo: repos.Repo) -> Path:
        """Where on the runner a machine mounts `repo`'s objects from."""

    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, cpus: int) -> None:
        """Create a stopped machine from `archive`, its mounts read-only; `memory_mib` None: smolvm's default."""

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        """Start a machine, its flows decided as `bottle`'s: served `served`'s remotes, with `features`' credentials.

        A machine never runs undecided: binding the bottle at the decider is
        part of starting it, under a token new for this run.
        """

    def release(self, bottle: str) -> None:
        """Stop deciding `bottle`'s flows, and forget its token; a no-op if it isn't bound."""

    def ensure_deciding(self) -> None:
        """The decider up for bottles still running, which it binds again with the tokens they started with."""

    def stop(self, machine: str) -> None: ...

    def delete(self, machine: str) -> None:
        """Delete a machine; a no-op if there's none."""

    def state(self, machine: str) -> str | None:
        """"running", "stopped", "created", ..., or None if there's no such machine."""

    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        """Run a command in a running machine and return its output; `input` is its stdin."""

    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        """The command line that runs `argv` in a machine with stdin attached (and a TTY), for this process to run."""


class LocalRunner:
    """The driver itself: machines are smolvm's here, images are in $BOTTLE_HOME, the decider is bottled's."""

    name = LOCAL

    def ensure_ready(self) -> None:
        prereqs.ensure_smolvm()

    def archive(self, tag: str) -> Path | None:
        return images.archive_of(tag)

    def objects(self, repo: repos.Repo) -> Path:
        # The repo's own: it's on this machine's disk, so nothing is copied.
        return repos.objects_dir(repo)

    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, cpus: int) -> None:
        smolvm.create(machine, str(archive), [smolvm.Mount(m.source, m.target) for m in mounts], env, memory_mib, cpus)

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        decider, token = daemon.bind_decider(bottle, served, features)
        smolvm.start(machine, decider, token)

    def release(self, bottle: str) -> None:
        daemon.release_egress(bottle)

    def ensure_deciding(self) -> None:
        daemon.start()

    def stop(self, machine: str) -> None:
        smolvm.stop(machine)

    def delete(self, machine: str) -> None:
        smolvm.delete(machine)

    def state(self, machine: str) -> str | None:
        return smolvm.state(machine)

    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        return smolvm.exec_(machine, argv, user=user, workdir=workdir, input=input, env=env)

    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        return smolvm.exec_command(machine, argv, user=user, workdir=workdir, tty=tty, env=env)


_LOCAL = LocalRunner()


def get(name: str = LOCAL) -> Runner:
    """The runner named `name`."""
    if name != LOCAL:
        raise BottleError(f"no runner named {name!r}")
    return _LOCAL
