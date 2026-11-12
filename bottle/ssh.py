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
controllers stays out of the way. bottle-cli's sources go with it, and the
runner builds its own bottle-cli from them (see remote.py), with the Rust
that `bottle runner add` installs if it's missing.
"""

import hashlib
import io
import os
import platform
import shlex
import subprocess
import tarfile
from dataclasses import dataclass
from pathlib import Path

from bottle.errors import BottleError
from bottle.store import bottle_home

PACKAGE = Path(__file__).resolve().parent
REPO = PACKAGE.parent
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
    """A hash of the code a runner runs: every file of this package, less what Python caches, and bottle-cli's
    sources."""
    digest = hashlib.sha256()
    for path in _code_files():
        digest.update(str(path.relative_to(REPO)).encode() + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()[:16]


def _code_files() -> list[Path]:
    """What a runner is sent, each where it is under REPO."""
    from bottle import prereqs

    package = [p for p in PACKAGE.rglob("*") if p.is_file() and "__pycache__" not in p.parts]
    return sorted(package + prereqs.source_files())


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
    rust: str  # "ready", "target" for rustup without the musl target bottle-cli's agent needs, "" for none
    lacks: tuple[str, ...] = ()  # what building bottle-cli needs that isn't there, besides Rust

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
        if self.lacks:
            problems.append(f"{', '.join(self.lacks)}, to build bottle-cli (on Debian or Ubuntu: apt install "
                            f"{' '.join(DEBIAN_PACKAGES)})")
        return problems


# What building bottle-cli takes on a runner, besides Rust: a C compiler and make (libkrun's build, mke2fs's),
# musl-gcc and the kernel's headers (mke2fs, static), curl (downloads), and on x86_64 bsdtar or rpm2cpio
# (the kernel, out of Fedora's package; see xtask). As Debian packages them:
DEBIAN_PACKAGES = ("build-essential", "musl-tools", "linux-libc-dev", "curl", "libarchive-tools")

CHECK = r"""
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo kvm=yes; else echo kvm=no; fi
echo "python=$(python3 -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null)"
if command -v git >/dev/null 2>&1; then echo git=yes; else echo git=no; fi
PATH="$HOME/.cargo/bin:$PATH"
if command -v rustup >/dev/null 2>&1; then
  if rustup target list --installed 2>/dev/null | grep -qx "$(uname -m)-unknown-linux-musl"; then echo rust=ready
  else echo rust=target; fi
elif command -v cargo >/dev/null 2>&1; then echo rust=ready
else echo rust=; fi
lacks=
for tool in cc make musl-gcc curl; do command -v $tool >/dev/null 2>&1 || lacks="$lacks $tool"; done
[ -f /usr/include/linux/version.h ] || lacks="$lacks kernel-headers"
if [ "$(uname -m)" = x86_64 ] && ! command -v bsdtar >/dev/null 2>&1 && ! command -v rpm2cpio >/dev/null 2>&1; then
  lacks="$lacks bsdtar"
fi
echo "lacks=$lacks"
"""

# rustup into the runner's ~/.cargo and ~/.rustup, as rustup.rs would, with the musl target for its
# architecture; or, with rustup there, just the target. rustup-init is rustup's archived release, checked
# against the checksums kept here (as the rust feature's install.sh does).
INSTALL_RUST = r"""
set -e
PATH="$HOME/.cargo/bin:$PATH"
target="$(uname -m)-unknown-linux-musl"
if command -v rustup >/dev/null 2>&1; then
  rustup target add "$target"
  exit
fi
version=1.29.2
case "$(uname -m)" in
  x86_64) sha256=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4 ;;
  aarch64) sha256=a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697 ;;
  *) echo "no rustup for $(uname -m)" >&2; exit 1 ;;
esac
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
curl -fsSL -o "$dir/rustup-init" "https://static.rust-lang.org/rustup/archive/$version/$(uname -m)-unknown-linux-gnu/rustup-init"
echo "$sha256  $dir/rustup-init" | sha256sum -c --quiet -
chmod +x "$dir/rustup-init"
"$dir/rustup-init" -y --profile minimal -t "$target"
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

    def call(self, op: str, request: str) -> str:
        # Its own code, if it has it; else it's sent, and asked again.
        script = self._launch(op)
        result = self.run(script, request.encode())
        if result.returncode == NOT_SHIPPED:
            self.ship()
            result = self.run(script, request.encode())
        if result.returncode != 0:
            why = result.stderr.decode(errors="replace").strip()
            raise BottleError(f"runner {self.runner}: {op} failed (exit {result.returncode}){': ' + why if why else ''}")
        return result.stdout.decode()

    def stream(self, op: str, request: str, data) -> str:
        """Run `op` there, `request` its argument and `data` (a file) its stdin, its stderr this process's."""
        script = self._launch(op, request)
        for attempt in range(2):
            data.seek(0)
            result = subprocess.run([*self._ssh(tty=False), script], stdin=data, stdout=subprocess.PIPE)
            if result.returncode == SSH_FAILED:
                raise BottleError(f"runner {self.runner} is unreachable (see ssh's message above)")
            if result.returncode != NOT_SHIPPED or attempt:
                break
            self.ship()
        if result.returncode != 0:
            raise BottleError(f"runner {self.runner}: {op} failed (exit {result.returncode})")
        return result.stdout.decode()

    def _launch(self, op: str, *args: str) -> str:
        """The script that runs bottle.remote's `op` there, with its own code if it has it (else exit NOT_SHIPPED)."""
        launcher = "import sys; sys.path.insert(0, sys.argv.pop(1)); from bottle import remote; sys.exit(remote.main(sys.argv[1:]))"
        return (f"[ -f {_code_dir()}/.complete ] || exit {NOT_SHIPPED}; exec python3 -I -c {shlex.quote(launcher)} "
                f"{_code_dir()} {shlex.join([op, *args])}")

    def ship(self) -> None:
        """Copy this code to the runner, complete or not at all."""
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:gz") as tar:
            for path in _code_files():
                tar.add(path, str(path.relative_to(REPO)))
        code = _code_dir()
        script = (f"set -e; rm -rf {code}.partial; mkdir -p {code}.partial; tar -xzf - -C {code}.partial; "
                  f"rm -rf {code}; mv {code}.partial {code}; touch {code}/.complete")
        result = self.run(script, archive.getvalue())
        if result.returncode != 0:
            raise BottleError(f"copying bottle to runner {self.runner} failed: {result.stderr.decode(errors='replace').strip()}")

    def close(self) -> None:
        """Let go of the shared connection now, rather than when it's been idle for PERSIST; a no-op without one."""
        subprocess.run([binary(), "-o", f"ControlPath={self._control_path()}", "-O", "exit", "--", self.destination],
                       capture_output=True)

    def command(self, argv: list[str], tty: bool) -> list[str]:
        return [*self._ssh(tty=tty), shlex.join(argv)]

    def link(self) -> None:
        from bottle import daemon

        daemon.ensure_link(self.runner)

    def check(self) -> Check:
        """What the runner has of what it needs."""
        found = dict(line.split("=", 1) for line in self.run(CHECK).stdout.decode().splitlines() if "=" in line)
        return Check(found.get("os", ""), found.get("arch", ""), found.get("kvm") == "yes", found.get("python", ""),
                     found.get("git") == "yes", found.get("rust", ""), tuple(found.get("lacks", "").split()))

    def install_rust(self) -> None:
        """Install rustup, and the target bottle-cli's agent is built for, into the runner's home."""
        result = self.run(INSTALL_RUST)
        if result.returncode != 0:
            raise BottleError(f"installing Rust on runner {self.runner} failed: "
                              f"{result.stderr.decode(errors='replace').strip()}")

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
