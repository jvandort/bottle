"""Runners: the machines bottles run on, bottle-cli's (`bottle runner`).

This machine is `local`; another is a Linux machine bottle-cli reaches over ssh,
whose machines bottle-cli here names RUNNER/NAME (qualified) and whose images
and CAs are its bottle-cli's there (`bottle-cli on RUNNER`: the runner's cli).
What to build (an image's tag, its inputs and their hash) is worked out here, in
images.py and features.py; a runner keeps the images it's built and builds
those it's sent. A machine mounts a repo's objects from the repo itself here,
and on a runner from a bare mirror of it there, pushed into over bottle-cli's
ssh as the machine is made.
"""

import hashlib
import json
import os
import shlex
import subprocess
from pathlib import Path

from folio.bottle import daemon, krun, repos, services
from folio.bottle.errors import BottleError

LOCAL = "local"
# Where on a runner, under its user's home, folio keeps bare mirrors of the repos its machines mount objects from.
MIRRORS = ".folio/git"


class Runner:
    """Runner `name`: its machines, by their names there, and where bottle-cli runs for its images (cli)."""

    def __init__(self, name: str = LOCAL) -> None:
        self.name = name
        self.cli = krun.LOCAL if name == LOCAL else krun.RunnerCli(name)
        self._info: dict | None = None

    def qualified(self, machine: str) -> str:
        """`machine`, as bottle-cli here names it."""
        return machine if self.name == LOCAL else f"{self.name}/{machine}"

    def _home(self, ssh: list[str]) -> str:
        """The home directory there of the user bottle-cli reaches it as."""
        found = subprocess.run([*ssh, 'printf %s "$HOME"'], capture_output=True, text=True)
        if found.returncode != 0 or not found.stdout:
            raise BottleError(f"runner {self.name}: {krun._error(found.stderr) or 'no home'}")
        return found.stdout

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

    def objects(self, repo: repos.Repo) -> Path:
        """Where on it a machine mounts `repo`'s objects from: the repo's own here, so nothing is copied; on a
        runner, a bare mirror of it there, brought up to date by a push from here. Fetches aren't served from the
        mirror: they come here, as everything routed does."""
        if self.name == LOCAL:
            return repos.objects_dir(repo)
        git_dir = repos.objects_dir(repo).parent
        ssh = [krun.binary(), "runner", "ssh", self.name]
        there = f"{self._home(ssh)}/{MIRRORS}/{mirror_key(git_dir)}.git"
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

    # Its machines, as krun.py has them, by their names there.

    def create(self, machine: str, image: str, mounts: list[krun.Mount], env: dict[str, str],
               memory_mib: int | None, disk_gib: int | None = None, user: str | None = None,
               workdir: str | None = None, labels: dict[str, str] | None = None,
               network: dict | None = None) -> None:
        """A stopped machine from `image` (a tag there), with all its cores, as krun.create makes it; `network`
        its policy (services.network), its routes to folio serve's sockets here, which bottle-cli forwards to a
        runner's."""
        krun.create(self.qualified(machine), image, mounts, env, memory_mib, self.cpus(), disk_gib, user,
                    workdir, labels, services.with_sockets(network, daemon.services_sockets()) if network else None)

    def configure(self, machine: str, network: dict) -> None:
        krun.configure(self.qualified(machine), services.with_sockets(network, daemon.services_sockets()))

    def start(self, machine: str) -> None:
        krun.start(self.qualified(machine))

    def stop(self, machine: str) -> None:
        krun.stop(self.qualified(machine))

    def rename(self, machine: str, new: str) -> None:
        krun.rename(self.qualified(machine), self.qualified(new))

    def delete(self, machine: str) -> None:
        krun.delete(self.qualified(machine))

    def state(self, machine: str) -> str | None:
        return krun.state(self.qualified(machine))

    def running(self) -> list[str]:
        """Every machine running there, bottles' or not: its own, not those of runners it reaches."""
        return [m for m in krun.running(cli=self.cli) if "/" not in m]

    def exec_(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              input: str | None = None, env: dict[str, str] | None = None) -> str:
        return krun.exec_(self.qualified(machine), argv, user=user, workdir=workdir, input=input, env=env)

    def exec_command(self, machine: str, argv: list[str], user: str | None = None, workdir: str | None = None,
                     tty: bool = False, env: dict[str, str] | None = None) -> list[str]:
        return krun.exec_command(self.qualified(machine), argv, user=user, workdir=workdir, tty=tty, env=env)



def mirror_key(git_dir: Path) -> str:
    """A repo's name among a runner's mirrors: its name, and a hash of where it is here."""
    name = git_dir.parent.name if git_dir.name == ".git" else git_dir.name.removesuffix(".git")
    return f"{name}-{hashlib.sha256(str(git_dir).encode()).hexdigest()[:8]}"


def registered() -> dict[str, dict]:
    """Every remote runner added (`bottle runner add`), by name, as `bottle runner ls` has them: each one's ssh
    destination, and whether it's detached."""
    listed = json.loads(krun.LOCAL.run(["runner", "ls", "--json"]).stdout or "[]")
    return {runner["name"]: runner for runner in listed}


def is_detached(name: str) -> bool:
    """Whether `name` was detached (`bottle runner detach`), to be turned off: nothing asks it anything until it's
    used again, which attaches it."""
    return name != LOCAL and bool(registered().get(name, {}).get("detached"))


_LOCAL = Runner()


def get(name: str = LOCAL) -> Runner:
    """The runner named `name`: this machine, or one added with `bottle runner add`."""
    if name == LOCAL:
        return _LOCAL
    if name not in registered():
        raise BottleError(f"no runner named {name!r}; add one with `bottle runner add {name} --ssh DESTINATION`")
    return Runner(name)
