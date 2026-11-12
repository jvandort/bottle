"""Runners: the machines bottles run on, behind one interface (see the README's `bottle runner`).

A runner holds bottles' machines, the images they boot, and the decider their
connections are asked about. Everything else -- the registry, credentials,
the repos bottles are served -- is the driver's, the machine bottle's commands
run on. The code on the driver's side reaches machines only through a
Runner, so it can't tell which runner it has.

The local runner is the driver itself: krun.py (bottle-cli's machines),
images.py and bottled, called in-process. Its decider is bottled's.

Code on the runner's side (krun.py, images.py's store and builds, the
decider) uses its neighbours directly: a builder machine is the runner's own
business. What to build (an image's tag, its inputs and their hash) is the
controller's to work out, in images.py and features.py; the runner keeps the
images it's built and builds those it's sent.
"""

import json
import os
import re
import sys
import tarfile
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Protocol

from bottle import daemon, images, krun, prereqs, repos
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

    def memory_mib(self) -> int:
        """How much memory the runner has, in MiB: what a bottle's spec there must fit."""

    def archive(self, tag: str) -> Path | None:
        """The disk of the top stage `tag` was last built into, in the runner's store; None if it isn't built there."""

    def image_labels(self, tag: str) -> dict[str, str] | None:
        """The labels `tag` was last built with there; None if it isn't built there."""

    def built(self) -> dict[str, dict[str, str]]:
        """Every tag built there, with its labels."""

    def build(self, context: Path, tag: str, inputs: str, recipe: dict, no_cache: bool = False) -> None:
        """Build `recipe` into `tag` there, `context` mounted for its steps (see images.build_recipe), its progress
        on this process's stderr."""

    def forget(self, tag: str) -> None:
        """Drop `tag` from the runner's index of images; its stages go once nothing uses them."""

    def remove_unused(self, in_use: set[str]) -> None:
        """Delete the runner's stages no tag's image is made of and no machine (`in_use`, disk names) was made from."""

    def objects(self, repo: repos.Repo) -> Path:
        """Where on the runner a machine mounts `repo`'s objects from."""

    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, disk_gib: int | None = None, user: str | None = None,
               workdir: str | None = None, labels: dict[str, str] | None = None) -> None:
        """Create a stopped machine from `archive` (a top stage's disk), its mounts read-only, with all the runner's
        cores; `memory_mib` and `disk_gib` None: bottle-cli's defaults; `user`, `workdir` and `labels` as
        krun.create has them."""

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        """Start a machine, its flows decided as `bottle`'s: served `served`'s remotes, with `features`' credentials.

        A machine never runs undecided: binding the bottle at the decider is
        part of starting it, under a token new for this run.
        """

    def release(self, bottle: str) -> None:
        """Stop deciding `bottle`'s flows, and forget its token; a no-op if it isn't bound."""

    def cache_certificate(self, bottle: str, hosts: list[str]) -> str | None:
        """The certificate of the CA the runner answers `bottle`'s cached `hosts` behind, made if missing; None
        where the controller's CA for the bottle does (here) or there are no hosts."""

    def forget_certificates(self, bottle: str) -> None:
        """Delete the runner's CA for `bottle`, if it has one."""

    def ensure_deciding(self) -> None:
        """The decider up for bottles still running, which it binds again with the tokens they started with."""

    def stop(self, machine: str) -> None: ...

    def rename(self, machine: str, new: str) -> None:
        """Rename a stopped machine."""

    def running(self) -> list[str]:
        """Every machine running there, bottles' or not (a builder, say)."""

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
    """The driver itself: machines are bottle-cli's here, images are in $BOTTLE_HOME, the decider is bottled's."""

    name = LOCAL

    def ensure_ready(self) -> None:
        prereqs.ensure_cli()

    def memory_mib(self) -> int:
        from bottle import specs

        return specs.host_memory_mib()

    def archive(self, tag: str) -> Path | None:
        return images.archive_of(tag)

    def image_labels(self, tag: str) -> dict[str, str] | None:
        return images.built_labels(tag)

    def built(self) -> dict[str, dict[str, str]]:
        return images.built()

    def build(self, context: Path, tag: str, inputs: str, recipe: dict, no_cache: bool = False) -> None:
        images.build_recipe(context, tag, inputs, recipe, no_cache)

    def forget(self, tag: str) -> None:
        images.forget(tag)

    def remove_unused(self, in_use: set[str]) -> None:
        images.remove_unused(in_use)

    def objects(self, repo: repos.Repo) -> Path:
        # The repo's own: it's on this machine's disk, so nothing is copied.
        return repos.objects_dir(repo)

    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, disk_gib: int | None = None, user: str | None = None,
               workdir: str | None = None, labels: dict[str, str] | None = None) -> None:
        krun.create(machine, str(archive), [krun.Mount(m.source, m.target) for m in mounts], env, memory_mib,
                    os.cpu_count() or 1, disk_gib, user, workdir, labels)

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        decider, token = daemon.bind_decider(bottle, served, features)
        krun.start(machine, decider, token)

    def release(self, bottle: str) -> None:
        daemon.release_egress(bottle)

    def cache_certificate(self, bottle: str, hosts: list[str]) -> str | None:
        return None  # the controller's CA for the bottle covers its cached hosts too

    def forget_certificates(self, bottle: str) -> None:
        pass

    def ensure_deciding(self) -> None:
        daemon.start()

    def stop(self, machine: str) -> None:
        krun.stop(machine)

    def rename(self, machine: str, new: str) -> None:
        krun.rename(machine, new)

    def running(self) -> list[str]:
        return krun.running()

    def delete(self, machine: str) -> None:
        krun.delete(machine)

    def state(self, machine: str) -> str | None:
        return krun.state(machine)

    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        return krun.exec_(machine, argv, user=user, workdir=workdir, input=input, env=env)

    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        return krun.exec_command(machine, argv, user=user, workdir=workdir, tty=tty, env=env)


class Transport(Protocol):
    """How a controller reaches a remote runner: an operation's request and answer, and commands run there."""

    def call(self, op: str, request: str) -> str:
        """Run `op` there (bottle.remote) with `request` on its stdin; its answer, from its stdout."""

    def command(self, argv: list[str], tty: bool) -> list[str]:
        """The command line that runs `argv` there, attached to this process's stdin (and a TTY)."""

    def link(self) -> None:
        """The runner's link to the controller up, so its bottled can reach the controller's."""

    def stream(self, op: str, request: str, data: BinaryIO) -> str:
        """Run `op` there with `request` as its argument and `data` on its stdin, its stderr on this process's:
        for what's long and worth watching, a build."""

    def ensure_dist(self) -> None:
        """bottle-cli there, as this repo's sources are now."""


class RemoteRunner:
    """A runner elsewhere: its own local runner, operated through `transport` (see bottle/remote.py)."""

    def __init__(self, name: str, transport: Transport) -> None:
        self.name = name
        self.transport = transport

    def _call(self, op: str, **args):
        from bottle import link

        if "served" in args:
            args["served"] = link.served_to_json(args["served"])
        answer = json.loads(self.transport.call(op, json.dumps(args)))
        if not answer.get("ok"):
            raise BottleError(f"runner {self.name}: {answer.get('error', 'failed')}")
        return answer.get("result")

    def ensure_ready(self) -> None:
        # Its bottle-cli as this repo's sources are now, built here; what else it's missing is the controller's to ask
        # about when the runner's added.
        self.transport.ensure_dist()

    def memory_mib(self) -> int:
        return self._call("memory_mib")

    def archive(self, tag: str) -> Path | None:
        found = self._call("archive", tag=tag)
        return Path(found) if found is not None else None

    def image_labels(self, tag: str) -> dict[str, str] | None:
        return self._call("image_labels", tag=tag)

    def built(self) -> dict[str, dict[str, str]]:
        return self._call("built")

    def build(self, context: Path, tag: str, inputs: str, recipe: dict, no_cache: bool = False) -> None:
        # The context sent as a tar, the recipe with the request. Built on images there, by a VM there, whose
        # connections are decided there and so through the controller.
        with tempfile.TemporaryFile() as sent:
            with tarfile.open(fileobj=sent, mode="w") as tar:
                tar.add(context, "context")
            sent.seek(0)
            self.transport.link()
            request = {"tag": tag, "inputs": inputs, "no_cache": no_cache, "recipe": recipe}
            answer = json.loads(self.transport.stream("build", json.dumps(request), sent))
        if not answer.get("ok"):
            raise BottleError(f"runner {self.name}: {answer.get('error', 'build failed')}")

    def forget(self, tag: str) -> None:
        self._call("forget", tag=tag)

    def remove_unused(self, in_use: set[str]) -> None:
        self._call("remove_unused", in_use=sorted(in_use))

    def objects(self, repo: repos.Repo) -> Path:
        from bottle import mirrors

        # Its mirror there, made if it's the first bottle of that hub there, and fetched through the link.
        self.transport.link()
        return Path(self._call("mirror", key=mirrors.key(repos.objects_dir(repo).parent)))

    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, disk_gib: int | None = None, user: str | None = None,
               workdir: str | None = None, labels: dict[str, str] | None = None) -> None:
        self._call("create", machine=machine, archive=str(archive),
                   mounts=[{"source": str(m.source), "target": m.target} for m in mounts],
                   env=env, memory_mib=memory_mib, disk_gib=disk_gib, user=user, workdir=workdir, labels=labels)

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        # Binding it there asks the controller, over the link.
        self.transport.link()
        self._call("start", machine=machine, bottle=bottle, served=tuple(served), features=list(features))

    def release(self, bottle: str) -> None:
        self._call("release", bottle=bottle)

    def cache_certificate(self, bottle: str, hosts: list[str]) -> str | None:
        return self._call("cache_ca", bottle=bottle, hosts=hosts)

    def forget_certificates(self, bottle: str) -> None:
        self._call("forget_ca", bottle=bottle)

    def ensure_deciding(self) -> None:
        self.transport.link()
        self._call("ensure_deciding")

    def stop(self, machine: str) -> None:
        self._call("stop", machine=machine)

    def rename(self, machine: str, new: str) -> None:
        self._call("rename", machine=machine, new=new)

    def running(self) -> list[str]:
        return self._call("running")

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
    """Every remote runner added, by name: each one's {"ssh": destination}, and "detached" if it is (see detach)."""
    return (read_json(registry_path()) or {}).get("runners", {})


def is_detached(name: str) -> bool:
    """Whether `name` was detached, to be turned off: nothing asks it anything until it's attached again."""
    return "detached" in registered().get(name, {})


def _mark(name: str, detached: dict | None) -> None:
    runners = registered()
    runners[name].pop("detached", None)
    if detached is not None:
        runners[name]["detached"] = detached
    write_json(registry_path(), {"version": 1, "runners": runners})


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

    if "detached" in entry:
        # Used again: it's attached again if it's back, and if it isn't, that's what's said.
        try:
            attach(name)
        except BottleError as e:
            raise BottleError(f"runner {name} is detached, and doesn't answer yet ({e}): "
                              "turn it on, then try again") from None
        print(f"bottle: runner {name} is back, and attached again", file=sys.stderr)
    return RemoteRunner(name, ssh.SshTransport(name, entry["ssh"]))


def add(name: str, destination: str) -> "ssh.Check":
    """Add a remote runner: check it over ssh, set up bottle there, and record it.

    What bottle can't set up itself refuses it, saying what's missing. bottle's
    code is copied there, and bottle-cli, built here for its architecture (on a
    Mac, in a Linux VM: see xtask), before it's recorded.
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
    transport.ship()
    transport.ensure_dist()
    register(name, destination)
    return check


def detach(name: str, offline: bool = False, ask=None) -> list[str]:
    """Stop everything on remote runner `name`, so it can be turned off, and mark it detached: until it's attached
    again, it isn't asked anything (its bottles show as detached), and its link and shared connection are let go.
    Returns the bottles stopped, which attach names to start again.

    `ask` is asked, given the bottles running there, whether to stop them; without it, or told no, nothing is
    stopped and it stays attached, so their agents can be told to wrap up first. Refused too, and left attached,
    if it can't be reached, or if a machine that isn't a bottle's (a build, say) still runs there: it isn't safe
    to turn off yet. `offline`: it's off already; mark it without asking it.
    """
    from bottle import bottles, ssh

    entry = registered().get(name)
    if name == LOCAL:
        raise BottleError("the local runner is this machine; it can't be detached")
    if entry is None:
        raise BottleError(f"no runner named {name!r}")
    if "detached" in entry:
        return []
    stopped: list[str] = []
    if not offline:
        runner = get(name)
        try:
            running = [b.name for b in bottles.load().values()
                       if b.runner == name and b.status == "ready" and runner.state(b.machine) == "running"]
        except BottleError as e:
            raise BottleError(f"{e}; if it's off already, `bottle runner detach {name} --offline`") from None
        if running and (ask is None or not ask(running)):
            raise BottleError(f"runner {name} is still attached, and {', '.join(running)} still running: "
                              f"`bottle runner detach {name}` again when they can be stopped")
        stopped, unreachable = bottles.stop_all(name)
        if unreachable:
            raise BottleError(f"{unreachable[name]}; if it's off already, `bottle runner detach {name} --offline`")
        still = get(name).running()
        if still:
            raise BottleError(f"runner {name} still runs {', '.join(still)}, which no bottle here stops: "
                              f"wait for it to finish, then `bottle runner detach {name}` again")
    _mark(name, {"at": time.time(), "stopped": stopped})
    ssh.SshTransport(name, entry["ssh"]).close()
    return stopped


def attach(name: str) -> list[str]:
    """Attach a detached runner again, once it answers; the bottles detaching it stopped, still there to start."""
    from bottle import bottles, ssh

    entry = registered().get(name)
    if entry is None:
        raise BottleError(f"no runner named {name!r}")
    ssh.SshTransport(name, entry["ssh"]).check()
    stopped = entry.get("detached", {}).get("stopped", [])
    if "detached" in entry:
        _mark(name, None)
    there = bottles.load()
    return [b for b in stopped if b in there and there[b].runner == name]


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
        if "detached" in remotes[name]:
            rows.append(row | {"detached": True, "reachable": None})
            continue
        try:
            check = ssh.SshTransport(name, row["ssh"]).check()
        except BottleError as e:
            rows.append(row | {"reachable": False, "error": str(e)})
            continue
        rows.append(row | {"reachable": True, "os": check.os, "arch": check.arch, "missing": check.missing()})
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
