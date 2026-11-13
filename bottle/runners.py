"""Runners: the machines bottles run on, behind one interface (see the README's `bottle runner`).

A runner holds bottles' machines, the images they boot, and the bottled their
machines route connections to. Everything else -- the registry, credentials,
the repos bottles are served -- is the driver's, the machine bottle's commands
run on. The code on the driver's side reaches machines only through a
Runner, so it can't tell which runner it has.

The local runner is the driver itself: krun.py (bottle-cli's machines),
images.py and bottled, called in-process. A remote runner is the same, its
bottle-cli run over ssh (ssh.RemoteCli): it has bottle-cli and nothing else of
bottle's. What to build (an image's tag, its inputs and their hash) is the
controller's to work out, in images.py and features.py; the runner keeps the
images it's built and builds those it's sent.
"""

import hashlib
import os
import re
import shlex
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Protocol

from bottle import daemon, images, krun, prereqs, repos, services
from bottle.errors import BottleError
from bottle.host import Served
from bottle.store import bottle_home, read_json, write_json

if TYPE_CHECKING:
    from bottle import ssh

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


class LocalRunner:
    """The driver itself: machines are bottle-cli's here, images are in $BOTTLE_HOME, services are bottled's."""

    name = LOCAL
    # Where bottle-cli runs for it: here.
    cli: krun.Cli = krun.LOCAL

    def ensure_ready(self) -> None:
        prereqs.ensure_cli()

    def memory_mib(self) -> int:
        from bottle import specs

        return specs.host_memory_mib()

    def cpus(self) -> int:
        return os.cpu_count() or 1

    def services_socket(self) -> str:
        """Where its machines reach bottled: what their routes say."""
        return str(daemon.services_socket())

    def archive(self, tag: str) -> Path | None:
        return images.archive_of(tag, self.cli)

    def image_labels(self, tag: str) -> dict[str, str] | None:
        return images.built_labels(tag, self.cli)

    def built(self) -> dict[str, dict[str, str]]:
        return images.built(self.cli)

    def build(self, context: Path, tag: str, inputs: str, recipe: dict, no_cache: bool = False) -> None:
        images.build_recipe(context, tag, inputs, recipe, no_cache, cli=self.cli)

    def forget(self, tag: str) -> None:
        images.forget(tag, self.cli)

    def remove_unused(self, in_use: set[str]) -> None:
        images.remove_unused(in_use, self.cli)

    def objects(self, repo: repos.Repo) -> Path:
        # The repo's own: it's on this machine's disk, so nothing is copied.
        return repos.objects_dir(repo)

    def create(self, machine: str, archive: Path, mounts: list[Mount], env: dict[str, str],
               memory_mib: int | None, disk_gib: int | None = None, user: str | None = None,
               workdir: str | None = None, labels: dict[str, str] | None = None,
               network: dict | None = None) -> None:
        krun.create(machine, str(archive), [krun.Mount(m.source, m.target) for m in mounts], env, memory_mib,
                    self.cpus(), disk_gib, user, workdir, labels,
                    services.with_socket(network, self.services_socket()) if network else None, cli=self.cli)

    def configure(self, machine: str, network: dict) -> None:
        krun.configure(machine, services.with_socket(network, self.services_socket()), cli=self.cli)

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        daemon.bind(bottle, machine, served, features)
        krun.start(machine, cli=self.cli)

    def release(self, bottle: str) -> None:
        daemon.release_egress(bottle)

    def cache_certificate(self, machine: str, hosts: list[str]) -> str | None:
        return krun.ca_certificate(machine, hosts, cli=self.cli)

    def ensure_deciding(self) -> None:
        daemon.start()

    def stop(self, machine: str) -> None:
        krun.stop(machine, cli=self.cli)

    def rename(self, machine: str, new: str) -> None:
        krun.rename(machine, new, cli=self.cli)

    def running(self) -> list[str]:
        return krun.running(cli=self.cli)

    def delete(self, machine: str) -> None:
        krun.delete(machine, cli=self.cli)

    def state(self, machine: str) -> str | None:
        return krun.state(machine, cli=self.cli)

    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        return krun.exec_(machine, argv, user=user, workdir=workdir, input=input, env=env, cli=self.cli)

    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        return krun.exec_command(machine, argv, user=user, workdir=workdir, tty=tty, env=env, cli=self.cli)


class RemoteRunner(LocalRunner):
    """A runner elsewhere, reached over ssh: its bottle-cli run there (ssh.RemoteCli), as the local runner runs
    this machine's; its machines' routes to a socket there that ssh forwards to bottled here (see daemon.Links);
    a repo's objects, for its machines to mount, pushed into a bare mirror there."""

    def __init__(self, name: str, transport: "ssh.SshTransport") -> None:
        from bottle import ssh

        self.name = name
        self.transport = transport
        self.cli = ssh.RemoteCli(transport)
        self._info: dict | None = None

    def ensure_ready(self) -> None:
        # Its bottle-cli as this repo's sources are now, built here; what else it's missing is the controller's to ask
        # about when the runner's added.
        self.transport.ensure_dist()

    def _host(self) -> dict:
        if self._info is None:
            self._info = krun.info(cli=self.cli)
        return self._info

    def memory_mib(self) -> int:
        return int(self._host()["memory_mib"])

    def cpus(self) -> int:
        return int(self._host()["cpus"])

    def services_socket(self) -> str:
        return self.transport.services_socket()

    def objects(self, repo: repos.Repo) -> Path:
        # A bare mirror of the repo there, brought up to date by a push from here: what its machines borrow
        # objects from. Fetches aren't served from it: they come here, as everything routed does.
        git_dir = repos.objects_dir(repo).parent
        there = f"{self.cli.home()}/git/{mirror_key(git_dir)}.git"
        made = self.transport.run(f"[ -d {shlex.quote(there)} ] || git init -q --bare {shlex.quote(there)}")
        if made.returncode != 0:
            raise BottleError(f"runner {self.name}: {made.stderr.decode(errors='replace').strip()}")
        # Its ssh as bottle runs it, up to "--": git adds the destination and its command, and nothing else (a
        # "simple" ssh, to git, takes no options of git's).
        ssh_command = shlex.join(self.transport._ssh(tty=False)[:-1])
        pushed = subprocess.run(["git", "-C", str(git_dir), "push", "--quiet", "--mirror",
                                 f"{self.transport.destination}:{there}"], capture_output=True, text=True,
                                env={**os.environ, "GIT_SSH_COMMAND": ssh_command, "GIT_SSH_VARIANT": "simple"})
        if pushed.returncode != 0:
            raise BottleError(f"runner {self.name}: copying {repo.name} there failed: {pushed.stderr.strip()}")
        return Path(there) / "objects"

    def start(self, machine: str, bottle: str, served: tuple[Served, ...] = (),
              features: tuple[str, ...] | list[str] = ()) -> None:
        # The forward its routes reach bottled through, up first.
        self.ensure_deciding()
        super().start(machine, bottle, served, features)

    def ensure_deciding(self) -> None:
        # bottled here, and the forward its machines reach it through.
        daemon.ensure_link(self.name)


def mirror_key(git_dir: Path) -> str:
    """A repo's name among a runner's mirrors: its name, and a hash of where it is here."""
    name = git_dir.parent.name if git_dir.name == ".git" else git_dir.name.removesuffix(".git")
    return f"{name}-{hashlib.sha256(str(git_dir).encode()).hexdigest()[:8]}"


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

    What bottle can't set up itself refuses it, saying what's missing.
    bottle-cli, built here for its architecture (on a Mac, in a Linux VM: see
    xtask), is copied there before it's recorded: all a runner has of bottle.
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
