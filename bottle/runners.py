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
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from bottle import daemon, images, prereqs, repos, smolvm
from bottle.errors import BottleError
from bottle.host import Served
from bottle.store import bottle_home, read_json, write_json

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
# A runner's name: like a bottle's, so it's safe in a path and a shell.
NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


def registry_path() -> Path:
    return bottle_home() / "runners.json"


def registered() -> dict[str, dict]:
    """Every remote runner added, by name: each one's {"ssh": destination}."""
    return (read_json(registry_path()) or {}).get("runners", {})


def register(name: str, destination: str) -> None:
    if name == LOCAL or not NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid runner name {name!r}: use letters, digits, '.', '_' or '-', and not {LOCAL!r}")
    runners = registered()
    runners[name] = {"ssh": destination}
    write_json(registry_path(), {"version": 1, "runners": runners})


def forget(name: str) -> None:
    runners = registered()
    if runners.pop(name, None) is not None:
        write_json(registry_path(), {"version": 1, "runners": runners})


def get(name: str = LOCAL) -> Runner:
    """The runner named `name`: this machine, or one added with `bottle runner add`."""
    if name == LOCAL:
        return _LOCAL
    entry = registered().get(name)
    if entry is None:
        raise BottleError(f"no runner named {name!r}; add one with `bottle runner add {name} --ssh DESTINATION`")
    from bottle import ssh

    return RemoteRunner(name, ssh.SshTransport(name, entry["ssh"]))


def add(name: str, destination: str, ask=None) -> "ssh.Check":
    """Add a remote runner: check it over ssh, set up what bottle can (asking first), and record it.

    `ask` is asked whether to install smolvm there, given what it's about to
    do; without it, a runner missing smolvm is refused. What bottle can't set
    up itself refuses it, saying what's missing.
    """
    from bottle import ssh

    if name == LOCAL or not NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid runner name {name!r}: use letters, digits, '.', '_' or '-', and not {LOCAL!r}")
    known = registered().get(name)
    if known is not None and known["ssh"] != destination:
        raise BottleError(f"runner {name} is already {known['ssh']}; `bottle runner remove {name}` first")
    transport = ssh.SshTransport(name, destination)
    check = transport.check()
    missing = check.missing()
    if missing:
        raise BottleError(f"{destination} can't be a runner yet; it needs: {'; '.join(missing)}")
    if not check.smolvm:
        question = f"{destination} has no smolvm. Install its latest release into ~/.bottle/smolvm there?"
        if ask is None or not ask(question):
            raise BottleError(f"{destination} needs smolvm")
        transport.install_smolvm()
        check = transport.check()
    transport.ship()
    register(name, destination)
    return check


def status(names: list[str] = ()) -> list[dict]:
    """Each runner, or those named: where it is, whether it can be reached and run bottles, and its bottles."""
    from bottle import bottles, ssh

    on: dict[str, list[str]] = {}
    for bottle in bottles.load().values():
        on.setdefault(bottle.runner, []).append(bottle.name)
    remotes = registered()
    rows = []
    for name in names or [LOCAL, *sorted(remotes)]:
        row: dict = {"name": name, "bottles": sorted(on.get(name, []))}
        if name == LOCAL:
            rows.append(row | {"ssh": None, "reachable": True})
            continue
        if name not in remotes:
            rows.append(row | {"state": "missing"})
            continue
        row["ssh"] = remotes[name]["ssh"]
        try:
            check = ssh.SshTransport(name, row["ssh"]).check()
        except BottleError as e:
            rows.append(row | {"reachable": False, "error": str(e)})
            continue
        rows.append(row | {"reachable": True, "os": check.os, "arch": check.arch, "smolvm": check.smolvm,
                           "missing": check.missing() + ([] if check.smolvm else ["smolvm"])})
    return rows


def remove(name: str) -> None:
    """Forget a remote runner with no bottles. Nothing on it is touched."""
    from bottle import bottles

    if name == LOCAL:
        raise BottleError("the local runner is this machine; it can't be removed")
    if name not in registered():
        raise BottleError(f"no runner named {name!r}")
    on = sorted(b.name for b in bottles.load().values() if b.runner == name)
    if on:
        raise BottleError(f"runner {name} has bottles: {', '.join(on)}; delete them first")
    forget(name)
