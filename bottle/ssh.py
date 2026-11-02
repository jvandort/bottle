"""Remote runners over ssh. All `ssh` invocations live here.

A remote runner is reached as the user's own ssh reaches it: a destination
from their ~/.ssh/config, or user@host, with their keys and agent. bottle
never asks for a password or a host key: ssh runs in batch mode, and a
runner it can't reach without asking is unreachable. Each runner gets one
shared connection (ssh's ControlMaster), kept for a while after its last
use, so the commands bottle runs there don't each log in again.

bottle's own code runs there too (bottle/remote.py), copied over the first
time it's needed into ~/.bottle/runner/<hash>/, named by its content's hash:
a runner runs exactly the code its controller does, and code from older
controllers stays out of the way.
"""

import hashlib
import io
import os
import shlex
import subprocess
import tarfile
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError
from bottle.store import bottle_home

PACKAGE = Path(__file__).resolve().parent
# ssh's own failures, connecting or authenticating, exit 255.
SSH_FAILED = 255
# What a runner's end exits when the code it's asked to run isn't there yet.
NOT_SHIPPED = 75
# How long a runner's shared connection outlives its last use, in seconds.
PERSIST = 600
# How long to wait for a runner to answer at all, in seconds: one that's off is said to be, soon.
CONNECT_TIMEOUT = 10
# macOS caps Unix socket paths at 104 bytes; a control socket's is ssh's to extend, by up to 17.
MAX_CONTROL_PATH = 80


def binary() -> str:
    """The ssh bottle runs: $BOTTLE_SSH if set, else `ssh` on PATH."""
    return os.environ.get("BOTTLE_SSH") or "ssh"


def code_hash() -> str:
    """A hash of the code a runner runs: every file of this package, less what Python caches."""
    digest = hashlib.sha256()
    for path in sorted(_code_files()):
        digest.update(str(path.relative_to(PACKAGE)).encode() + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()[:16]


def _code_files() -> list[Path]:
    return [p for p in PACKAGE.rglob("*") if p.is_file() and "__pycache__" not in p.parts]


def _code_dir() -> str:
    """Where a runner has this code, as its shell says it (so ~ is its own home)."""
    return f'"$HOME/.bottle/runner/{code_hash()}"'


@dataclass(frozen=True)
class Check:
    """What a runner has, of what it needs to run bottles."""

    os: str
    arch: str
    kvm: bool
    python: str  # its python3's version, "3.13"; "" for none
    git: bool
    curl: bool
    smolvm: str  # "bottle" for the one bottle installed, "path" for one on its PATH, "" for none

    def missing(self) -> list[str]:
        """What it lacks that bottle can't set up itself, each as the user would fix it."""
        problems = []
        if self.os != "Linux":
            problems.append(f"Linux (it runs {self.os or 'something else'})")
        if not self.kvm:
            problems.append("access to /dev/kvm (add the user to the kvm group)")
        if not self.python or tuple(int(n) for n in self.python.split(".")) < (3, 11):
            problems.append(f"Python 3.11 or later as python3 (it has {self.python or 'none'})")
        if not self.git:
            problems.append("git")
        if not self.curl and not self.smolvm:
            problems.append("curl, to install smolvm")
        return problems


CHECK = r"""
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo kvm=yes; else echo kvm=no; fi
echo "python=$(python3 -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null)"
if command -v git >/dev/null 2>&1; then echo git=yes; else echo git=no; fi
if command -v curl >/dev/null 2>&1; then echo curl=yes; else echo curl=no; fi
if [ -x "$HOME/.bottle/smolvm/current/smolvm" ]; then echo smolvm=bottle
elif command -v smolvm >/dev/null 2>&1; then echo smolvm=path
else echo smolvm=; fi
"""

# smolvm's latest Linux release for the runner's architecture, its checksum checked, into ~/.bottle/smolvm.
INSTALL_SMOLVM = r"""
set -e
case "$(uname -m)" in
  x86_64) arch=x86_64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "no smolvm release for $(uname -m)" >&2; exit 1 ;;
esac
version=$(curl -fsSL https://api.github.com/repos/smol-machines/smolvm/releases/latest |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["tag_name"].removeprefix("v"))')
release="https://github.com/smol-machines/smolvm/releases/download/v$version"
archive="smolvm-$version-linux-$arch.tar.gz"
mkdir -p "$HOME/.bottle/smolvm"
cd "$HOME/.bottle/smolvm"
curl -fsSLO "$release/$archive"
curl -fsSL "$release/checksums.sha256" | grep "  $archive\$" | sha256sum -c --quiet -
tar -xzf "$archive"
rm "$archive"
ln -sfn "smolvm-$version-linux-$arch" current
echo "$version"
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
                "-o", f"ControlPersist={PERSIST}", *({True: ["-t"], False: ["-T"], None: []}[tty]),
                "--", self.destination]

    def run(self, script: str, input: bytes | str | None = None) -> subprocess.CompletedProcess:
        """Run `script` in the runner's shell; refused as unreachable if ssh couldn't get there."""
        result = subprocess.run([*self._ssh(tty=False), script], input=input, capture_output=True)
        if result.returncode == SSH_FAILED:
            why = result.stderr.decode(errors="replace").strip().splitlines()
            raise BottleError(f"runner {self.runner} is unreachable ({why[-1] if why else 'ssh failed'})")
        return result

    def call(self, op: str, request: str) -> str:
        # Its own code, if it has it; else it's sent, and asked again.
        launcher = "import sys; sys.path.insert(0, sys.argv.pop(1)); from bottle import remote; sys.exit(remote.main(sys.argv[1:]))"
        script = (f"[ -f {_code_dir()}/.complete ] || exit {NOT_SHIPPED}; "
                  f"exec python3 -I -c {shlex.quote(launcher)} {_code_dir()} {shlex.quote(op)}")
        result = self.run(script, request.encode())
        if result.returncode == NOT_SHIPPED:
            self.ship()
            result = self.run(script, request.encode())
        if result.returncode != 0:
            why = result.stderr.decode(errors="replace").strip()
            raise BottleError(f"runner {self.runner}: {op} failed (exit {result.returncode}){': ' + why if why else ''}")
        return result.stdout.decode()

    def ship(self) -> None:
        """Copy this code to the runner, complete or not at all."""
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:gz") as tar:
            for path in _code_files():
                tar.add(path, str(Path("bottle") / path.relative_to(PACKAGE)))
        code = _code_dir()
        script = (f"set -e; rm -rf {code}.partial; mkdir -p {code}.partial; tar -xzf - -C {code}.partial; "
                  f"rm -rf {code}; mv {code}.partial {code}; touch {code}/.complete")
        result = self.run(script, archive.getvalue())
        if result.returncode != 0:
            raise BottleError(f"copying bottle to runner {self.runner} failed: {result.stderr.decode(errors='replace').strip()}")

    def command(self, argv: list[str], tty: bool) -> list[str]:
        return [*self._ssh(tty=tty), shlex.join(argv)]

    def link(self) -> None:
        from bottle import daemon

        daemon.ensure_link(self.runner)

    def check(self) -> Check:
        """What the runner has of what it needs."""
        found = dict(line.split("=", 1) for line in self.run(CHECK).stdout.decode().splitlines() if "=" in line)
        return Check(found.get("os", ""), found.get("arch", ""), found.get("kvm") == "yes", found.get("python", ""),
                     found.get("git") == "yes", found.get("curl") == "yes", found.get("smolvm", ""))

    def install_smolvm(self) -> str:
        """Install smolvm's latest release into the runner's ~/.bottle/smolvm; its version."""
        result = self.run(INSTALL_SMOLVM)
        if result.returncode != 0:
            raise BottleError(f"installing smolvm on runner {self.runner} failed: "
                              f"{result.stderr.decode(errors='replace').strip()}")
        return result.stdout.decode().strip()

    # --- the link (see link.py) ------------------------------------------------------

    def prepare_link(self) -> str:
        """Where the runner's link.sock is, as an absolute path, with any left by a link that died removed."""
        result = self.run('mkdir -p "$HOME/.bottle" && rm -f "$HOME/.bottle/link.sock" && printf %s "$HOME"')
        if result.returncode != 0:
            raise BottleError(f"runner {self.runner}: {result.stderr.decode(errors='replace').strip()}")
        return f"{result.stdout.decode()}/.bottle/link.sock"

    def forward_command(self, there: str, here: Path) -> list[str]:
        """The ssh that forwards the runner's `there` to the controller's socket `here`, for as long as it can.

        A connection of its own, not the shared one: it lives as long as the
        link does. It ends if the forward can't be made, or if the runner
        stops answering for 45 seconds.
        """
        return [binary(), "-N", "-T", "-o", "BatchMode=yes", "-o", f"ConnectTimeout={CONNECT_TIMEOUT}",
                "-o", "ExitOnForwardFailure=yes",
                "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", "-o", "ControlMaster=no",
                "-o", "ControlPath=none", "-R", f"{there}:{here}", "--", self.destination]

    def wait_for_link(self, there: str, process, timeout: float = 15) -> bool:
        """Whether the forwarded socket appeared on the runner before `process`, the forwarding ssh, ended."""
        import time

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline and process.returncode is None:
            if self.run(f"[ -S {shlex.quote(there)} ]").returncode == 0:
                return True
            time.sleep(0.2)
        return False
