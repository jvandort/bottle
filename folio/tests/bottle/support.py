import os
import platform
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


class GitTestCase(unittest.TestCase):
    """Runs each test in a temp dir with BOTTLE_HOME and git config isolated.

    The user's global git config is ignored so tests never sign commits,
    prompt for passphrases, or pick up hooks and aliases.
    """

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        # Resolve so paths match what git reports (/var is a symlink on macOS).
        self.tmp = Path(tmp.name).resolve()
        self.bottle_home = self.tmp / "bottle-home"
        env = mock.patch.dict(
            os.environ,
            {
                "BOTTLE_HOME": str(self.bottle_home),
                "FOLIO_HOME": str(self.tmp / "folio-home"),
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_AUTHOR_NAME": "Test",
                "GIT_AUTHOR_EMAIL": "test@example.com",
                "GIT_COMMITTER_NAME": "Test",
                "GIT_COMMITTER_EMAIL": "test@example.com",
            },
        )
        env.start()
        self.addCleanup(env.stop)

    def make_repo(self, name: str = "project", origin: str | None = None) -> Path:
        repo = self.tmp / name
        run("git", "init", "-q", "-b", "main", repo)
        self.commit(repo, "initial")
        if origin:
            run("git", "-C", repo, "remote", "add", "origin", origin)
        return repo

    def commit(self, repo: Path, message: str) -> str:
        run("git", "-C", repo, "commit", "-q", "--allow-empty", "-m", message)
        return run("git", "-C", repo, "rev-parse", "HEAD")


def header(address: str, port: int, name: str | None = None, tls: bool = False, machine: str | None = "b-m") -> bytes:
    """What bottle-cli writes before a routed connection's bytes (agent-client's net.rs, proxy_header): a PROXY
    protocol v2 header, with its fields (the name the bottle asked for, whether it spoke TLS, its machine)."""
    import ipaddress

    ip = ipaddress.ip_address(address)
    body = bytes(len(ip.packed)) + ip.packed + bytes(2) + port.to_bytes(2, "big")
    for kind, value in ((0x02, name.encode() if name else None), (0x20, bytes([0x01, 0, 0, 0, 0]) if tls else None),
                        (0xE0, machine.encode() if machine else None)):
        if value is not None:
            body += bytes([kind]) + len(value).to_bytes(2, "big") + value
    family = 0x11 if ip.version == 4 else 0x21
    return b"\r\n\r\n\x00\r\nQUIT\n" + bytes([0x21, family]) + len(body).to_bytes(2, "big") + body


async def bridge(socket_path: "Path", address: str, port: int, machine: str, name: str | None = None):
    """A TCP server on 127.0.0.1 standing in for a bottle's machine: each connection carried to folio serve's
    socket at `socket_path` as bottle-cli carries one the machine routes there, after a PROXY header saying it was
    to `address`:`port` (`name`) from `machine`. Returns the server and its port."""
    import asyncio

    async def pipe(reader, writer) -> None:
        try:
            while data := await reader.read(65536):
                writer.write(data)
                await writer.drain()
            if writer.can_write_eof():
                writer.write_eof()
        except (ConnectionError, OSError):
            pass

    async def carry(reader, writer) -> None:
        there_reader, there_writer = await asyncio.open_unix_connection(str(socket_path))
        there_writer.write(header(address, port, name, machine=machine))
        await asyncio.gather(pipe(reader, there_writer), pipe(there_reader, writer))
        writer.close()
        there_writer.close()

    server = await asyncio.start_server(carry, "127.0.0.1", 0)
    return server, server.sockets[0].getsockname()[1]


def run(*args: str | Path) -> str:
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True, check=True)
    return result.stdout.strip()


def krun_present() -> bool:
    """Whether bottle-cli ($BOTTLE_CLI, else the one `cargo xtask dist` built here) is here, on a Mac to run VMs."""
    from folio.bottle import krun

    return platform.system() == "Darwin" and os.path.isfile(krun.binary())


# For tests that run real machines: they need bottle-cli and a Mac, and skip elsewhere.
requires_krun = unittest.skipUnless(krun_present(), "needs bottle-cli on a Mac (cargo xtask dist, or $BOTTLE_CLI)")


_BUILT: dict[str, Path] = {}


def _built(package: str) -> Path:
    """`package`'s program, built from this repo by cargo (debug), for tests that drive the real one; skips the test
    where cargo can't build it."""
    import shutil

    if package in _BUILT:
        return _BUILT[package]
    repo = Path(__file__).resolve().parents[3]
    cargo = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / "cargo")
    if not Path(cargo).exists():
        raise unittest.SkipTest(f"needs cargo, to build {package}")
    built = subprocess.run([cargo, "build", "-q", "-p", package], cwd=repo, capture_output=True, text=True)
    if built.returncode != 0:
        raise unittest.SkipTest(f"{package} didn't build: {built.stderr.strip()[-300:]}")
    target = Path(os.environ.get("CARGO_TARGET_DIR") or repo / "target")
    _BUILT[package] = target / "debug" / package
    return _BUILT[package]


def built_cli() -> Path:
    """bottle-cli, built from this repo."""
    return _built("bottle-cli")


def built_folio_cli() -> Path:
    """folio-cli, built from this repo."""
    return _built("folio-cli")


def write_bottles(bottles_: dict) -> None:
    """Every bottle's record, replaced by `bottles_`'s ({name: Bottle}), in thread_dir()."""
    from folio.bottle import bottles

    for path in bottles.records().values():
        path.unlink()
    for bottle in bottles_.values():
        bottles._save(bottle, thread_dir())


def register_runner(name: str, destination: str) -> None:
    """Runner `name` added, at ssh `destination`, as `folio runner add` records it (unchecked)."""
    from folio.bottle.store import folio_home, read_json, write_json

    path = folio_home() / "runners.json"
    known = (read_json(path) or {}).get("runners", {})
    write_json(path, {"version": 1, "runners": {**known, name: {"ssh": destination}}})


def thread_dir() -> "Path":
    """A thread's directory in folio's home, for the bottles a test makes: where their records are kept."""
    return Path(os.environ["FOLIO_HOME"]) / "repo-0" / "thread"


def stop_bottle(name: str) -> None:
    """Stop a bottle's machine, as bottle-cli's stop does, and folio serve's answering it: what a test stops one
    with, to start it again."""
    from folio.bottle import bottles, daemon

    bottle = bottles.get(name)
    runner = bottles.runner_of(bottle)
    if runner.state(bottle.machine) == "running":
        runner.stop(bottle.machine)
    daemon.release_egress(bottle.name)


def manifest(dist: "Path") -> None:
    """A distribution's manifest, as xtask writes it: each file's sha256 and name, as sha256sum writes them."""
    import hashlib

    names = sorted(p.name for p in dist.iterdir() if p.is_file() and not p.name.startswith("."))
    (dist / ".files").write_text("".join(f"{hashlib.sha256((dist / n).read_bytes()).hexdigest()}  {n}\n"
                                         for n in names))
