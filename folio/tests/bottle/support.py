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


def manifest(dist: "Path") -> None:
    """A distribution's manifest, as xtask writes it: each file's sha256 and name, as sha256sum writes them."""
    import hashlib

    names = sorted(p.name for p in dist.iterdir() if p.is_file() and not p.name.startswith("."))
    (dist / ".files").write_text("".join(f"{hashlib.sha256((dist / n).read_bytes()).hexdigest()}  {n}\n"
                                         for n in names))
