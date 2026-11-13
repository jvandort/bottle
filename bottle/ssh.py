"""Remote runners over ssh. All `ssh` invocations live here.

A remote runner is reached as the user's own ssh reaches it: a destination
from their ~/.ssh/config, or user@host, with their keys and agent. bottle
never asks for a password or a host key: ssh runs in batch mode, and a
runner it can't reach without asking is unreachable. Each runner gets one
shared connection (ssh's ControlMaster), kept for a while after its last
use, so the commands bottle runs there don't each log in again.

A runner has bottle-cli and nothing else of bottle's: the controller builds
the distribution for the runner's architecture (see prereqs.ensure_dist) and
copies it to ~/.bottle/dist there whenever it was built from other sources,
then runs it there over ssh (RemoteCli), as krun runs the controller's own.
What its machines route to bottled reaches the controller's, through a
socket there that ssh forwards back (forward_command).
"""

import hashlib
import io
import json
import os
import platform
import shlex
import subprocess
import sys
import tarfile
from dataclasses import dataclass
from pathlib import Path

from bottle import krun
from bottle.errors import BottleError
from bottle.store import bottle_home

PACKAGE = Path(__file__).resolve().parent
# ssh's own failures, connecting or authenticating, exit 255.
SSH_FAILED = 255
# How long a runner's shared connection outlives its last use, in seconds.
PERSIST = 600
# How long to wait for a runner to answer at all, in seconds: one that's off is said to be, soon.
CONNECT_TIMEOUT = 10
# macOS caps Unix socket paths at 104 bytes; a control socket's is ssh's to extend, by up to 17.
MAX_CONTROL_PATH = 80


# What macOS says to a process it doesn't let reach the local network, whatever the network is doing.
NO_ROUTE = "No route to host"
LOCAL_NETWORK = ("on macOS, often the Local Network permission (System Settings > Privacy & Security > Local "
                 "Network) of whatever started this: bottled has the permission of the process that started it")


def explained(why: str) -> str:
    """What ssh said, with the likeliest reason on a Mac when it's that there's no route."""
    if NO_ROUTE in why and platform.system() == "Darwin":
        return f"{why}; {LOCAL_NETWORK}"
    return why


def binary() -> str:
    """The ssh bottle runs: $BOTTLE_SSH if set, else `ssh` on PATH."""
    return os.environ.get("BOTTLE_SSH") or "ssh"


def code_hash() -> str:
    """A hash of bottle's code: every file of this package, less what Python caches (what bottled runs, see
    daemon)."""
    digest = hashlib.sha256()
    for path in sorted(_code_files()):
        digest.update(str(path.relative_to(PACKAGE)).encode() + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()[:16]


def _code_files() -> list[Path]:
    return [p for p in PACKAGE.rglob("*") if p.is_file() and "__pycache__" not in p.parts]


@dataclass(frozen=True)
class Check:
    """What a runner has, of what it needs to run bottles."""

    os: str
    arch: str
    kvm: bool
    git: bool

    def missing(self) -> list[str]:
        """What it lacks that bottle can't set up itself, each as the user would fix it."""
        from bottle import prereqs

        problems = []
        if self.os != "Linux":
            problems.append(f"Linux (it runs {self.os or 'something else'})")
        elif self.arch not in prereqs.LINUX_TARGETS:
            problems.append(f"x86_64 or arm64 (it's {self.arch or 'something else'})")
        if not self.kvm:
            problems.append("access to /dev/kvm (add the user to the kvm group)")
        if not self.git:
            problems.append("git")
        return problems


CHECK = r"""
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo kvm=yes; else echo kvm=no; fi
if command -v git >/dev/null 2>&1; then echo git=yes; else echo git=no; fi
"""

# Where a runner's bottle-cli is, and what it was built from.
DIST = ".bottle/dist"
# Where the runner's machines reach bottled: forwarded to the controller's services socket.
SERVICES = ".bottle/services.sock"
DIST_STAMP = r"""
echo "arch=$(uname -m)"
echo "sources=$(cat "$HOME/.bottle/dist/.sources" 2>/dev/null)"
"""


class SshTransport:
    """How the controller reaches one remote runner (see runners.Transport)."""

    def __init__(self, runner: str, destination: str) -> None:
        self.runner = runner
        self.destination = destination

    def _control_path(self) -> Path:
        path = bottle_home() / "ssh" / f"{self.runner}.sock"
        if len(str(path)) <= MAX_CONTROL_PATH:
            return path
        key = hashlib.sha256(str(path).encode()).hexdigest()[:16]
        return Path(f"/tmp/bottle-{os.getuid()}/{key}.sock")

    def _ssh(self, tty: bool | None = None) -> list[str]:
        control = self._control_path()
        control.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        return [binary(), "-o", "BatchMode=yes", "-o", f"ConnectTimeout={CONNECT_TIMEOUT}",
                "-o", "ControlMaster=auto", "-o", f"ControlPath={control}",
                "-o", f"ControlPersist={PERSIST}",
                # A shared connection to a runner that went away (rebooted, say) is given up in 45s, not hung on.
                "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3",
                *({True: ["-t"], False: ["-T"], None: []}[tty]),
                "--", self.destination]

    def run(self, script: str, input: bytes | str | None = None) -> subprocess.CompletedProcess:
        """Run `script` in the runner's shell; refused as unreachable if ssh couldn't get there."""
        result = subprocess.run([*self._ssh(tty=False), script], input=input, capture_output=True)
        if result.returncode == SSH_FAILED:
            why = result.stderr.decode(errors="replace").strip().splitlines()
            raise BottleError(f"runner {self.runner} is unreachable ({explained(why[-1] if why else 'ssh failed')})")
        return result

    def close(self) -> None:
        """Let go of the shared connection now, rather than when it's been idle for PERSIST; a no-op without one."""
        subprocess.run([binary(), "-o", f"ControlPath={self._control_path()}", "-O", "exit", "--", self.destination],
                       capture_output=True)

    def command(self, argv: list[str], tty: bool) -> list[str]:
        return [*self._ssh(tty=tty), shlex.join(argv)]

    def check(self) -> Check:
        """What the runner has of what it needs."""
        found = dict(line.split("=", 1) for line in self.run(CHECK).stdout.decode().splitlines() if "=" in line)
        return Check(found.get("os", ""), found.get("arch", ""), found.get("kvm") == "yes", found.get("git") == "yes")

    def ensure_dist(self) -> None:
        """The runner's bottle-cli, in ~/.bottle/dist there, built from this repo's sources as they are now: if it
        was built from others, or isn't there, the distribution for its architecture is built here (see
        prereqs.ensure_dist) and copied over, replacing the old whole. Its bottles running keep the old one."""
        from bottle import prereqs

        found = dict(line.split("=", 1) for line in self.run(DIST_STAMP).stdout.decode().splitlines() if "=" in line)
        target = prereqs.LINUX_TARGETS.get(found.get("arch", ""))
        if target is None:
            raise BottleError(f"runner {self.runner} is {found.get('arch') or 'unknown'}: bottle-cli is built for "
                              "x86_64 and arm64")
        if found.get("sources") == prereqs.source_hash():
            return
        dist = prereqs.ensure_dist(target)
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:gz") as tar:
            tar.add(dist, ".")
        there = f'"$HOME/{DIST}"'
        script = (f"set -e; rm -rf {there}.partial; mkdir -p {there}.partial; tar -xzf - -C {there}.partial; "
                  f"rm -rf {there}; mv {there}.partial {there}")
        print(f"bottle: copying bottle-cli to runner {self.runner}...", file=sys.stderr)
        result = self.run(script, archive.getvalue())
        if result.returncode != 0:
            raise BottleError(f"copying bottle-cli to runner {self.runner} failed: "
                              f"{result.stderr.decode(errors='replace').strip()}")

    def home(self) -> str:
        """$HOME there, absolute."""
        result = self.run('printf %s "$HOME"')
        if result.returncode != 0 or not result.stdout:
            raise BottleError(f"runner {self.runner}: {result.stderr.decode(errors='replace').strip()}")
        return result.stdout.decode()

    # --- what its machines route to bottled --------------------------------------------

    def services_socket(self) -> str:
        """Where the runner's machines reach the controller's bottled: a socket ssh forwards (forward_command)."""
        return f"{self.home()}/{SERVICES}"

    def prepare_forward(self) -> str:
        """The runner's services socket, with any left by a forward that died removed."""
        there = self.services_socket()
        result = self.run(f"mkdir -p {shlex.quote(str(Path(there).parent))} && rm -f {shlex.quote(there)}")
        if result.returncode != 0:
            raise BottleError(f"runner {self.runner}: {result.stderr.decode(errors='replace').strip()}")
        return there

    def forward_command(self, there: str, here: Path) -> list[str]:
        """The ssh that forwards the runner's `there` to the controller's socket `here`, for as long as it can.

        A connection of its own, not the shared one: it lives as long as the
        forward does. It ends if the forward can't be made, or if the runner
        stops answering for 45 seconds.
        """
        return [binary(), "-N", "-T", "-o", "BatchMode=yes", "-o", f"ConnectTimeout={CONNECT_TIMEOUT}",
                "-o", "ExitOnForwardFailure=yes",
                "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", "-o", "ControlMaster=no",
                "-o", "ControlPath=none", "-R", f"{there}:{here}", "--", self.destination]

    def wait_for_forward(self, there: str, process, timeout: float = 15) -> bool:
        """Whether the forwarded socket appeared on the runner before `process`, the forwarding ssh, ended."""
        import time

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline and process.returncode is None:
            if self.run(f"[ -S {shlex.quote(there)} ]").returncode == 0:
                return True
            time.sleep(0.2)
        return False


class RemoteCli(krun.Cli):
    """bottle-cli on a runner, run over its shared ssh connection: what krun and images run there."""

    def __init__(self, transport: SshTransport) -> None:
        self.transport = transport
        self._home: str | None = None

    def command(self, args: list[str], tty: bool | None = None) -> list[str]:
        return [*self.transport._ssh(tty=tty), f'"$HOME/{DIST}/bottle-cli" {shlex.join(args)}']

    def home(self) -> str:
        if self._home is None:
            self._home = f"{self.transport.home()}/.bottle"
        return self._home

    def run(self, args: list[str], input: str | None = None) -> subprocess.CompletedProcess:
        result = subprocess.run(self.command(args, tty=False), capture_output=True, text=True, input=input)
        # bottle-cli's own failures exit 255 too, as ssh's do: told apart by what it says.
        why = result.stderr.strip().splitlines()
        if result.returncode == SSH_FAILED and not any(line.startswith("bottle-cli: ") for line in why):
            raise BottleError(f"runner {self.transport.runner} is unreachable "
                              f"({explained(why[-1] if why else 'ssh failed')})")
        return result

    def build(self, recipe: dict, network: dict, context: Path, store: str, tag: str) -> subprocess.CompletedProcess:
        # Sent as a tar, the recipe mounting the context beside it, and built from a directory there.
        sent = io.BytesIO()
        with tarfile.open(fileobj=sent, mode="w") as tar:
            for name, value in (("recipe.json", {**recipe, "mount": {**recipe["mount"], "source": "context"}}),
                                ("network.json", network)):
                data = json.dumps(value).encode()
                info = tarfile.TarInfo(name)
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
            tar.add(context, "context")
        script = (f'set -e; d=$(mktemp -d); trap \'rm -rf "$d"\' EXIT; tar -xf - -C "$d"; '
                  f'"$HOME/{DIST}/bottle-cli" image build "$d/recipe.json" {shlex.quote(store)} '
                  f'--tag {shlex.quote(tag)} --network "$d/network.json"')
        result = subprocess.run([*self.transport._ssh(tty=False), script], input=sent.getvalue(),
                                stdout=subprocess.PIPE)
        # Failed, its own way or ssh's (both exit 255): whichever said so, above.
        return subprocess.CompletedProcess(result.args, result.returncode, result.stdout.decode(errors="replace"))

