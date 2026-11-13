"""Runners: the machines bottles run on, folio-cli's (`folio runner`).

This machine is `local`; another is a Linux machine folio-cli reaches over ssh,
whose bottle-cli (`folio-cli on RUNNER`: the runner's cli) has its machines, by
their own names, and its images and CAs. What to build (an image's tag, its
inputs and their hash) is worked out here, in images.py and features.py; a
runner keeps the images it's built and builds those it's sent. A machine mounts
a repo's objects from the repo itself here, and on a runner from a bare mirror
of it there, pushed into over folio-cli's ssh as the machine is made.

What a machine routes to folio serve goes to its sockets here (services.py);
on a runner, to sockets there, under its user's home (services_sockets), which
folio serve forwards here, over one ssh per runner, while it answers bottles
there (forwards; `folio-cli runner forward`). Those there are named for this
machine (controller_key), so two machines folio runs on can share a runner.
"""

import hashlib
import json
import os
import shlex
import socket
import subprocess
from pathlib import Path

from folio.bottle import daemon, krun, repos, services
from folio.bottle.errors import BottleError

LOCAL = "local"
# Where on a runner, under its user's home, folio keeps bare mirrors of the repos its machines mount objects from.
MIRRORS = ".folio/git"
# Where on a runner, under its user's home, the sockets folio serve's are forwarded to are, a directory for each
# machine folio runs on (controller_key).
SERVICES = ".folio/services"


class Runner:
    """Runner `name`: its machines, by their names there, and the bottle-cli that has them (cli)."""

    def __init__(self, name: str = LOCAL) -> None:
        self.name = name
        self.cli = krun.LOCAL if name == LOCAL else krun.RunnerCli(name)
        self._info: dict | None = None

    def _host(self) -> dict:
        if self._info is None:
            self._info = krun.info(cli=self.cli)
        return self._info

    def memory_mib(self) -> int:
        """How much memory it has, in MiB: what a bottle's spec there must fit."""
        if self.name == LOCAL:
            from folio.bottle import specs

            return specs.host_memory_mib()
        return int(self._host()["memory_mib"])

    def cpus(self) -> int:
        return os.cpu_count() or 1 if self.name == LOCAL else int(self._host()["cpus"])

    def services_sockets(self) -> dict[str, str]:
        """Where a machine on it reaches folio serve's services, by service: folio serve's sockets, here; on a
        runner, sockets there that folio serve forwards here (forwards)."""
        here = daemon.services_sockets()
        if self.name == LOCAL:
            return {name: str(path) for name, path in here.items()}
        there = f"{home(self.name)}/{SERVICES}/{controller_key()}"
        return {name: f"{there}/{name}.sock" for name in here}

    def forwards(self) -> list[str]:
        """What folio serve forwards to a runner while it answers bottles there, as `folio-cli runner forward`
        takes it: each of its services' sockets there, =, its own."""
        here = daemon.services_sockets()
        return [f"{there}={here[name]}" for name, there in self.services_sockets().items()]

    def objects(self, repo: repos.Repo) -> Path:
        """Where on it a machine mounts `repo`'s objects from: the repo's own here, so nothing is copied; on a
        runner, a bare mirror of it there, brought up to date by a push from here. Fetches aren't served from the
        mirror: they come here, as everything routed does."""
        if self.name == LOCAL:
            return repos.objects_dir(repo)
        git_dir = repos.objects_dir(repo).parent
        ssh = [krun.folio_binary(), "runner", "ssh", self.name]
        there = f"{home(self.name)}/{MIRRORS}/{mirror_key(git_dir)}.git"
        made = subprocess.run([*ssh, f"[ -d {shlex.quote(there)} ] || git init -q --bare {shlex.quote(there)}"],
                              capture_output=True, text=True)
        if made.returncode != 0:
            raise BottleError(f"runner {self.name}: {krun._error(made.stderr)}")
        # git runs folio-cli's ssh, a "simple" one to it: the runner's name for a host, and git's command.
        pushed = subprocess.run(["git", "-C", str(git_dir), "push", "--quiet", "--mirror", f"{self.name}:{there}"],
                                capture_output=True, text=True,
                                env={**os.environ, "GIT_SSH_COMMAND": shlex.join(ssh[:-1]),
                                     "GIT_SSH_VARIANT": "simple"})
        if pushed.returncode != 0:
            raise BottleError(f"runner {self.name}: copying {repo.name} there failed: {pushed.stderr.strip()}")
        return Path(there) / "objects"

    # Its machines, as krun.py has them, by their names there.

    def create(self, machine: str, image: str, mounts: list[krun.Mount], env: dict[str, str],
               memory_mib: int | None, disk_gib: int | None = None, user: str | None = None,
               workdir: str | None = None, labels: dict[str, str] | None = None,
               network: dict | None = None) -> None:
        """A stopped machine from `image` (a tag there), with all its cores, as krun.create makes it; `network`
        its policy (services.network), its routes to folio serve's services where it is (services_sockets)."""
        krun.create(machine, image, mounts, env, memory_mib, self.cpus(), disk_gib, user, workdir, labels,
                    services.with_sockets(network, self.services_sockets()) if network else None, cli=self.cli)

    def configure(self, machine: str, network: dict) -> None:
        krun.configure(machine, services.with_sockets(network, self.services_sockets()), cli=self.cli)

    def start(self, machine: str) -> None:
        krun.start(machine, cli=self.cli)

    def stop(self, machine: str) -> None:
        krun.stop(machine, cli=self.cli)

    def rename(self, machine: str, new: str) -> None:
        krun.rename(machine, new, cli=self.cli)

    def delete(self, machine: str) -> None:
        krun.delete(machine, cli=self.cli)

    def state(self, machine: str) -> str | None:
        return krun.state(machine, cli=self.cli)

    def running(self) -> list[str]:
        """Every machine running there, bottles' or not."""
        return krun.running(cli=self.cli)

    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        return krun.exec_(machine, argv, user=user, workdir=workdir, input=input, env=env, cli=self.cli)

    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        return krun.exec_command(machine, argv, user=user, workdir=workdir, tty=tty, env=env, cli=self.cli)


def mirror_key(git_dir: Path) -> str:
    """A repo's name among a runner's mirrors: its name, and a hash of where it is here."""
    name = git_dir.parent.name if git_dir.name == ".git" else git_dir.name.removesuffix(".git")
    return f"{name}-{hashlib.sha256(str(git_dir).encode()).hexdigest()[:8]}"


def _folio_cli(*args: str) -> str:
    """folio-cli's output, with `args`; raises BottleError, saying why, if it fails."""
    try:
        done = subprocess.run([krun.folio_binary(), *args], capture_output=True, text=True)
    except FileNotFoundError:
        raise BottleError(krun.RunnerCli("").missing()) from None
    if done.returncode != 0:
        raise BottleError(krun._error(done.stderr) or f"folio-cli {' '.join(args)} failed")
    return done.stdout


def registered() -> dict[str, dict]:
    """Every remote runner added (`folio runner add`), by name, as `folio-cli runner ls` has them: each one's ssh
    destination, and whether it's detached."""
    return {runner["name"]: runner for runner in json.loads(_folio_cli("runner", "ls", "--json") or "[]")}


def is_detached(name: str) -> bool:
    """Whether `name` was detached (`folio runner detach`), to be turned off: nothing asks it anything until it's
    used again, which attaches it."""
    return name != LOCAL and bool(registered().get(name, {}).get("detached"))


def home(name: str) -> str:
    """Runner `name`'s user's home there, as folio-cli keeps it (asking the runner once)."""
    return _folio_cli("runner", "home", name).strip()


def controller_key() -> str:
    """This machine, folio's home on it, among those folio runs on: what its sockets on a runner are named for."""
    from folio.bottle.store import folio_home

    return hashlib.sha256(f"{socket.gethostname()}\0{folio_home()}".encode()).hexdigest()[:8]


_LOCAL = Runner()


def get(name: str = LOCAL) -> Runner:
    """The runner named `name`: this machine, or one added with `folio runner add`."""
    if name == LOCAL:
        return _LOCAL
    if name not in registered():
        raise BottleError(f"no runner named {name!r}; add one with `folio runner add {name} --ssh DESTINATION`")
    return Runner(name)
