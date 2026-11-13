"""Runners: the machines bottles run on, behind one interface (bottle-cli's: `bottle runner`).

A runner holds bottles' machines, the images they boot, and the bottled their
machines route connections to. Everything else -- the registry, credentials,
the repos bottles are served -- is the driver's, the machine bottle's commands
run on. The code on the driver's side reaches machines only through a
Runner, so it can't tell which runner it has.

The local runner is the driver itself: krun.py (bottle-cli's machines),
images.py and bottled, called in-process. A remote runner is
bottle-cli's (`bottle-cli runner`), its machines this machine's bottle-cli's
by RUNNER/NAME: it has bottle-cli and nothing else of bottle's. What to build (an image's tag, its inputs and their hash) is the
controller's to work out, in images.py and features.py; the runner keeps the
images it's built and builds those it's sent.
"""

import functools
import hashlib
import os
import shlex
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from bottle import daemon, images, krun, prereqs, repos, services
from bottle.errors import BottleError
from bottle.host import Served
from bottle.store import bottle_home, read_json


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
               workdir: str | None = None, labels: dict[str, str] | None = None,
               network: dict | None = None) -> None:
        """Create a stopped machine from `archive` (a top stage's disk), its mounts read-only, with all the runner's
        cores; `memory_mib` and `disk_gib` None: bottle-cli's defaults; `user`, `workdir` and `labels` as
        krun.create has them; `network` its policy (services.network), its routes to bottled's socket there."""

    def configure(self, machine: str, network: dict) -> None:
        """Give a stopped machine its policy, as create does."""

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        """Start a machine, bottled there answering what it routes as `bottle`'s: served `served`'s remotes, with
        `features`' credentials, read now."""

    def release(self, bottle: str) -> None:
        """Stop deciding `bottle`'s flows, and forget its token; a no-op if it isn't bound."""

    def cache_certificate(self, machine: str, hosts: list[str]) -> str | None:
        """The certificate of the caching CA the runner's bottle-cli answers `machine`'s cached https `hosts`
        behind, made if missing; None if there are no hosts. bottle-cli deletes it with the machine."""

    def ensure_deciding(self) -> None:
        """bottled up there, for what bottles still running route to it: it binds them again as it starts."""

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


def _readied(method):
    """`method`, its runner made ready first (ensure_ready: its bottle-cli as this repo's sources are now), the
    first time any such method is called on it."""
    @functools.wraps(method)
    def readied(self, *args, **kwargs):
        if not getattr(self, "_is_ready", False):
            self.ensure_ready()
            self._is_ready = True
        return method(self, *args, **kwargs)
    return readied


class LocalRunner:
    """The driver itself: machines are bottle-cli's here, images are in $BOTTLE_HOME, services are bottled's."""

    name = LOCAL
    # Where bottle-cli keeps its images, and makes its CAs: here.
    cli: krun.Cli = krun.LOCAL

    def ensure_ready(self) -> None:
        prereqs.ensure_cli()

    @_readied
    def memory_mib(self) -> int:
        from bottle import specs

        return specs.host_memory_mib()

    def cpus(self) -> int:
        return os.cpu_count() or 1

    def qualified(self, machine: str) -> str:
        """`machine`, as this machine's bottle-cli names it."""
        return machine

    @_readied
    def archive(self, tag: str) -> Path | None:
        return images.archive_of(tag, self.cli)

    @_readied
    def image_labels(self, tag: str) -> dict[str, str] | None:
        return images.built_labels(tag, self.cli)

    @_readied
    def built(self) -> dict[str, dict[str, str]]:
        return images.built(self.cli)

    @_readied
    def build(self, context: Path, tag: str, inputs: str, recipe: dict, no_cache: bool = False) -> None:
        images.build_recipe(context, tag, inputs, recipe, no_cache, cli=self.cli)

    @_readied
    def forget(self, tag: str) -> None:
        images.forget(tag, self.cli)

    @_readied
    def remove_unused(self, in_use: set[str]) -> None:
        images.remove_unused(in_use, self.cli)

    def objects(self, repo: repos.Repo) -> Path:
        # The repo's own: it's on this machine's disk, so nothing is copied.
        return repos.objects_dir(repo)

    @_readied
    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, disk_gib: int | None = None, user: str | None = None,
               workdir: str | None = None, labels: dict[str, str] | None = None,
               network: dict | None = None) -> None:
        # Its routes to bottled's socket here, wherever the machine is: bottle-cli forwards them to a runner's.
        krun.create(self.qualified(machine), str(archive), [krun.Mount(m.source, m.target) for m in mounts], env,
                    memory_mib, self.cpus(), disk_gib, user, workdir, labels,
                    services.with_sockets(network, daemon.services_sockets()) if network else None)

    @_readied
    def configure(self, machine: str, network: dict) -> None:
        krun.configure(self.qualified(machine), services.with_sockets(network, daemon.services_sockets()))

    @_readied
    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        daemon.bind(bottle, machine, served, features)
        krun.start(self.qualified(machine))

    def release(self, bottle: str) -> None:
        daemon.release_egress(bottle)

    @_readied
    def cache_certificate(self, machine: str, hosts: list[str]) -> str | None:
        return krun.ca_certificate(machine, hosts, cli=self.cli)

    def ensure_deciding(self) -> None:
        daemon.start()

    @_readied
    def stop(self, machine: str) -> None:
        krun.stop(self.qualified(machine))

    @_readied
    def rename(self, machine: str, new: str) -> None:
        krun.rename(self.qualified(machine), self.qualified(new))

    @_readied
    def running(self) -> list[str]:
        # Its own, not those of runners it reaches.
        return [m for m in krun.running(cli=self.cli) if "/" not in m]

    @_readied
    def delete(self, machine: str) -> None:
        krun.delete(self.qualified(machine))

    @_readied
    def state(self, machine: str) -> str | None:
        return krun.state(self.qualified(machine))

    @_readied
    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        return krun.exec_(self.qualified(machine), argv, user=user, workdir=workdir, input=input, env=env)

    @_readied
    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        return krun.exec_command(self.qualified(machine), argv, user=user, workdir=workdir, tty=tty, env=env)


class RemoteRunner(LocalRunner):
    """A runner elsewhere: bottle-cli's (`bottle-cli runner`), its machines this machine's bottle-cli's by
    RUNNER/NAME, its images and CAs bottle-cli's there (`bottle-cli on RUNNER`); a repo's objects, for its machines
    to mount, pushed into a bare mirror there, over bottle-cli's ssh."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.cli = krun.RunnerCli(name)
        self._info: dict | None = None

    def _host(self) -> dict:
        if self._info is None:
            self._info = krun.info(cli=self.cli)
        return self._info

    @_readied
    def memory_mib(self) -> int:
        return int(self._host()["memory_mib"])

    @_readied
    def cpus(self) -> int:
        return int(self._host()["cpus"])

    def qualified(self, machine: str) -> str:
        return f"{self.name}/{machine}"

    @_readied
    def objects(self, repo: repos.Repo) -> Path:
        # A bare mirror of the repo there, brought up to date by a push from here: what its machines borrow
        # objects from. Fetches aren't served from it: they come here, as everything routed does.
        git_dir = repos.objects_dir(repo).parent
        there = f"{self.cli.home()}/git/{mirror_key(git_dir)}.git"
        ssh = [krun.binary(), "runner", "ssh", self.name]
        made = subprocess.run([*ssh, f"[ -d {shlex.quote(there)} ] || git init -q --bare {shlex.quote(there)}"],
                              capture_output=True, text=True)
        if made.returncode != 0:
            raise BottleError(f"runner {self.name}: {krun._error(made.stderr)}")
        # git runs bottle-cli's ssh, a "simple" one to it: the runner's name for a host, and git's command.
        pushed = subprocess.run(["git", "-C", str(git_dir), "push", "--quiet", "--mirror", f"{self.name}:{there}"],
                                capture_output=True, text=True,
                                env={**os.environ, "GIT_SSH_COMMAND": shlex.join(ssh[:-1]),
                                     "GIT_SSH_VARIANT": "simple"})
        if pushed.returncode != 0:
            raise BottleError(f"runner {self.name}: copying {repo.name} there failed: {pushed.stderr.strip()}")
        return Path(there) / "objects"


def mirror_key(git_dir: Path) -> str:
    """A repo's name among a runner's mirrors: its name, and a hash of where it is here."""
    name = git_dir.parent.name if git_dir.name == ".git" else git_dir.name.removesuffix(".git")
    return f"{name}-{hashlib.sha256(str(git_dir).encode()).hexdigest()[:8]}"


_LOCAL = LocalRunner()


def registry_path() -> Path:
    return bottle_home() / "runners.json"


def registered() -> dict[str, dict]:
    """Every remote runner added (`bottle runner add`), by name: each one's {"ssh": destination}, and "detached"
    if it is."""
    return (read_json(registry_path()) or {}).get("runners", {})


def is_detached(name: str) -> bool:
    """Whether `name` was detached (`bottle runner detach`), to be turned off: nothing asks it anything until it's
    used again, which attaches it."""
    return "detached" in registered().get(name, {})


def get(name: str = LOCAL) -> Runner:
    """The runner named `name`: this machine, or one added with `bottle runner add`."""
    if name == LOCAL:
        return _LOCAL
    if name not in registered():
        raise BottleError(f"no runner named {name!r}; add one with `bottle runner add {name} --ssh DESTINATION`")
    return RemoteRunner(name)
