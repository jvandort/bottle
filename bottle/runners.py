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

import json
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


class Transport(Protocol):
    """How a controller reaches a remote runner: an operation's request and answer, and commands run there."""

    def call(self, op: str, request: str) -> str:
        """Run `op` there (bottle.remote) with `request` on its stdin; its answer, from its stdout."""

    def command(self, argv: list[str], tty: bool) -> list[str]:
        """The command line that runs `argv` there, attached to this process's stdin (and a TTY)."""


class RemoteRunner:
    """A runner elsewhere: its own local runner, operated through `transport` (see bottle/remote.py)."""

    def __init__(self, name: str, transport: Transport) -> None:
        self.name = name
        self.transport = transport

    def _call(self, op: str, **args):
        from bottle import remote

        if "served" in args:
            args["served"] = remote.served_to_json(args["served"])
        answer = json.loads(self.transport.call(op, json.dumps(args)))
        if not answer.get("ok"):
            raise BottleError(f"runner {self.name}: {answer.get('error', 'failed')}")
        return answer.get("result")

    def ensure_ready(self) -> None:
        pass  # what it's missing is the controller's to ask about, and set up when the runner's added

    def archive(self, tag: str) -> Path | None:
        found = self._call("archive", tag=tag)
        return Path(found) if found is not None else None

    def objects(self, repo: repos.Repo) -> Path:
        raise BottleError(f"runner {self.name} has no mirrors yet, so no repo can be mounted there")

    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, cpus: int) -> None:
        self._call("create", machine=machine, archive=str(archive),
                   mounts=[{"source": str(m.source), "target": m.target} for m in mounts],
                   env=env, memory_mib=memory_mib, cpus=cpus)

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        self._call("start", machine=machine, bottle=bottle, served=tuple(served), features=list(features))

    def release(self, bottle: str) -> None:
        self._call("release", bottle=bottle)

    def ensure_deciding(self) -> None:
        self._call("ensure_deciding")

    def stop(self, machine: str) -> None:
        self._call("stop", machine=machine)

    def delete(self, machine: str) -> None:
        self._call("delete", machine=machine)

    def state(self, machine: str) -> str | None:
        return self._call("state", machine=machine)

    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        return self._call("exec", machine=machine, argv=argv, user=user, workdir=workdir, input=input, env=env)

    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        there = self._call("exec_command", machine=machine, argv=argv, user=user, workdir=workdir, tty=tty, env=env)
        return self.transport.command(there, tty)


_LOCAL = LocalRunner()


def get(name: str = LOCAL) -> Runner:
    """The runner named `name`."""
    if name != LOCAL:
        raise BottleError(f"no runner named {name!r}")
    return _LOCAL
