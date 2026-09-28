"""bottled: the host process that serves every bottle's egress proxy.

Commands don't manage it directly. Anything that needs a bottle's egress calls
ensure_egress(), which starts the daemon if it isn't running, like prereqs.
It listens on a Unix socket in $BOTTLE_HOME and speaks one JSON line per request.
"""

import asyncio
import hashlib
import json
import logging
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

from bottle import egress, runtime
from bottle.host import HostServices
from bottle.errors import BottleError
from bottle.store import bottle_home

log = logging.getLogger("bottle.daemon")
log.addHandler(logging.NullHandler())

# Each bottle's network has its own gateway address, so one port serves them all.
EGRESS_PORT = 3128
START_TIMEOUT = 5


# macOS caps Unix socket paths at 104 bytes.
MAX_SOCKET_PATH = 100


def socket_path() -> Path:
    path = bottle_home() / "bottled.sock"
    if len(str(path)) <= MAX_SOCKET_PATH:
        return path
    # A deep BOTTLE_HOME: use a short per-user directory, keyed by BOTTLE_HOME.
    key = hashlib.sha256(str(bottle_home()).encode()).hexdigest()[:16]
    return Path(f"/tmp/bottle-{os.getuid()}/{key}.sock")


def log_path() -> Path:
    return bottle_home() / "logs" / "bottled.log"


# --- client -------------------------------------------------------------------


def ensure_egress(bottle: str, network: str, git_dir: Path | None = None) -> str:
    """Make sure `bottle`'s egress proxy is serving; return its URL.

    `git_dir` is the repo the proxy serves the bottle as origin (see host.py).
    """
    message = {"op": "ensure", "bottle": bottle, "network": network, "git_dir": str(git_dir) if git_dir else None}
    return _request(message, start=True)["proxy"]


def release_egress(bottle: str) -> None:
    """Stop serving `bottle`'s egress, if the daemon is running at all."""
    _request({"op": "release", "bottle": bottle}, start=False)


def start() -> bool:
    """Start the daemon in the background. False if it was already running."""
    try:
        _send({"op": "ping"})
        return False
    except (FileNotFoundError, ConnectionRefusedError):
        _start_daemon()
        return True


def stop() -> bool:
    """Stop the daemon and every egress it serves. False if it wasn't running."""
    try:
        reply = _send({"op": "shutdown"})
    except (FileNotFoundError, ConnectionRefusedError):
        return False
    if not reply.get("ok"):
        raise BottleError(f"bottled: {reply.get('error', 'shutdown failed')}")
    return True


def _request(message: dict, start: bool) -> dict:
    try:
        reply = _send(message)
    except (FileNotFoundError, ConnectionRefusedError):
        if not start:
            return {"ok": True}
        _start_daemon()
        reply = _send(message)
    if not reply.get("ok"):
        raise BottleError(f"bottled: {reply.get('error', 'request failed')}")
    return reply


def _send(message: dict) -> dict:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(30)
        s.connect(str(socket_path()))
        s.sendall(json.dumps(message).encode() + b"\n")
        return json.loads(s.makefile().readline())


def _start_daemon() -> None:
    log_path().parent.mkdir(parents=True, exist_ok=True)
    launcher = Path(__file__).resolve().parent.parent / "bin" / "bottle"
    with open(log_path(), "a") as out:
        # A new session detaches it from this terminal, so it outlives the command.
        subprocess.Popen(
            [sys.executable, str(launcher), "daemon", "start", "--foreground"],
            stdin=subprocess.DEVNULL, stdout=out, stderr=out, start_new_session=True,
        )
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        try:
            _send({"op": "ping"})
            return
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.05)
    raise BottleError(f"bottled didn't start; see {log_path()}")


# --- server -------------------------------------------------------------------


class Daemon:
    def __init__(self, port: int = EGRESS_PORT, policy: egress.Policy | None = None) -> None:
        self.port = port
        self.policy = policy or egress.Policy()
        self.proxies: dict[str, tuple[str, asyncio.Server]] = {}
        self.stopping = asyncio.Event()
        # Serializes work per bottle: startup restore and requests may overlap.
        self.locks: dict[str, asyncio.Lock] = {}

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        line = await reader.readline()
        if not line:  # a liveness probe: connected and hung up
            writer.close()
            return
        try:
            message = json.loads(line)
            match message.get("op"):
                case "ping":
                    reply = {"ok": True}
                case "ensure":
                    git_dir = Path(message["git_dir"]) if message.get("git_dir") else None
                    reply = {"ok": True, "proxy": await self.ensure(message["bottle"], message["network"], git_dir)}
                case "release":
                    await self.release(message["bottle"])
                    reply = {"ok": True}
                case "shutdown":
                    self.stopping.set()
                    reply = {"ok": True}
                case op:
                    reply = {"ok": False, "error": f"unknown op {op!r}"}
        except BottleError as e:
            reply = {"ok": False, "error": str(e)}
        except Exception as e:
            log.exception("request failed")
            reply = {"ok": False, "error": f"internal error: {e}"}
        writer.write(json.dumps(reply).encode() + b"\n")
        await writer.drain()
        writer.close()

    async def ensure(self, bottle: str, network: str, git_dir: Path | None = None) -> str:
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            return await self._ensure(bottle, network, git_dir)

    async def _ensure(self, bottle: str, network: str, git_dir: Path | None) -> str:
        gateway = await asyncio.to_thread(runtime.network_gateway, network)
        if bottle in self.proxies:
            served, server = self.proxies[bottle]
            # A stopped bottle's network loses its gateway address; the old socket may be dead.
            if served == gateway and await _accepting(gateway, self.port):
                return proxy_url(gateway, self.port)
            self._close(bottle)
        try:
            services = HostServices(git_dir)
            server = await egress.EgressProxy(bottle, self.policy, services=services).start(gateway, self.port)
        except OSError as e:
            raise BottleError(f"can't serve {bottle}'s egress on {gateway}:{self.port}: {e.strerror}") from None
        self.proxies[bottle] = (gateway, server)
        log.info("%s: egress on %s:%d", bottle, gateway, self.port)
        return proxy_url(gateway, self.port)

    async def release(self, bottle: str) -> None:
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            self._close(bottle)

    def _close(self, bottle: str) -> None:
        if bottle in self.proxies:
            _, server = self.proxies.pop(bottle)
            server.close()
            log.info("%s: egress stopped", bottle)

    async def restore(self, running: list[tuple[str, str, Path | None]]) -> None:
        """Serve egress for bottles that are already running, e.g. after a restart."""
        for bottle, network, git_dir in running:
            try:
                await self.ensure(bottle, network, git_dir)
            except Exception as e:  # one bad bottle mustn't stop the rest
                log.warning("%s: couldn't restore egress: %s", bottle, e)


def running_bottles() -> list[tuple[str, str, Path | None]]:
    """(name, network, repo git dir) of every ready bottle whose container is running."""
    from bottle import bottles  # bottles imports this module

    return [(b.name, b.network, bottles.git_dir(b)) for b in bottles.load().values()
            if b.status == "ready" and runtime.container_state(b.container) == "running"]


async def _accepting(host: str, port: int) -> bool:
    try:
        _, writer = await asyncio.wait_for(asyncio.open_connection(host, port), 1)
    except (OSError, TimeoutError):
        return False
    writer.close()
    return True


async def _restore(daemon: Daemon) -> None:
    await daemon.restore(await asyncio.to_thread(running_bottles))


def proxy_url(host: str, port: int) -> str:
    return f"http://{host}:{port}"


async def serve(path: Path | None = None, daemon: Daemon | None = None) -> None:
    path = path or socket_path()
    daemon = daemon or Daemon()
    # Owner-only: anyone who can reach the socket can start proxies.
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.parent.stat().st_uid != os.getuid():
        raise BottleError(f"{path.parent} is owned by another user")
    if path.exists():
        try:
            _, writer = await asyncio.open_unix_connection(str(path))
            writer.close()
            log.info("another bottled is already running; exiting")
            return
        except (ConnectionRefusedError, FileNotFoundError):
            path.unlink()  # left over from a daemon that died
    server = await asyncio.start_unix_server(daemon.handle, str(path))
    os.chmod(path, 0o600)
    log.info("bottled %d listening on %s", os.getpid(), path)
    # In the background, so requests are answered while bottles are being restored.
    restoring = asyncio.create_task(_restore(daemon))
    try:
        async with server:
            await daemon.stopping.wait()
    finally:
        restoring.cancel()
        for bottle in list(daemon.proxies):
            await daemon.release(bottle)
        path.unlink(missing_ok=True)
        log.info("bottled %d stopped", os.getpid())
