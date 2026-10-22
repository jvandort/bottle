"""bottled: the host process that decides every bottle's connections.

It runs the decider (see decider.py) on host loopback, which smolvm asks
about each of a bottle's connections before making it. Commands don't manage
it directly: bind_decider() gives a bottle its token there, starting bottled
if it isn't running, and release_egress() takes it back. It listens on a Unix
socket in $BOTTLE_HOME and speaks one JSON line per request.
"""

import asyncio
import functools
import hashlib
import json
import logging
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

from bottle import auth, ca, decider as decider_, egress
from bottle.host import HostServices, Remote
from bottle.errors import BottleError
from bottle.store import bottle_home, log_path, open_log

log = logging.getLogger("bottle.daemon")
log.addHandler(logging.NullHandler())

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


# --- client -------------------------------------------------------------------


def bind_decider(
    bottle: str, git_dir: Path | None = None, features: tuple[str, ...] | list[str] = (),
    remotes: tuple[Remote, ...] = (),
) -> tuple[str, str]:
    """Have the decider answer `bottle`'s flows; return its address and the bottle's new token, in hex.

    The token goes to the driver that starts the bottle's machine, and nowhere
    else: whoever holds it can speak for the bottle.
    """
    message = {
        "op": "bind", "bottle": bottle, "git_dir": str(git_dir) if git_dir else None,
        "features": list(features), "remotes": [r.spec for r in remotes],
    }
    reply = _request(message, start=True)
    return reply["decider"], reply["token"]


def release_egress(bottle: str) -> None:
    """Stop answering `bottle`'s flows at the decider, and forget its token, if bottled is running at all."""
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
        log.info("asked bottled to stop")
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
    log.info("starting bottled")
    launcher = Path(__file__).resolve().parent.parent / "bin" / "bottle"
    # Its own records go to the log through logging, stamped; this catches anything else it prints.
    with os.fdopen(open_log("bottled"), "a") as out:
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
    raise BottleError(f"bottled didn't start; see {log_path('bottled')}")


# --- server -------------------------------------------------------------------


class Daemon:
    def __init__(
        self, policy: egress.Policy | None = None, decider: decider_.Decider | None = None,
        decider_port: int | None = None,
    ) -> None:
        self.policy = policy or egress.Policy()
        self.decider = decider or decider_.Decider()
        self.decider_port = decider_.port() if decider_port is None else decider_port
        # Where the decider listens once it does: host:port.
        self.decider_address: str | None = None
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
                case "bind":
                    git_dir = Path(message["git_dir"]) if message.get("git_dir") else None
                    remotes = tuple(Remote.parse(spec) for spec in message.get("remotes", ()))
                    token = await self.bind(message["bottle"], git_dir, tuple(message.get("features", ())), remotes)
                    reply = {"ok": True, "decider": self.decider_address, "token": token.hex()}
                case "release":
                    await self.release(message["bottle"])
                    reply = {"ok": True}
                case "shutdown":
                    log.info("shutdown requested")
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

    def _gateway(
        self, bottle: str, git_dir: Path | None, remotes: tuple[Remote, ...], injections: tuple[egress.Injection, ...],
        intercepted: tuple[str, ...],
    ) -> egress.Gateway:
        """What decides and serves `bottle`'s flows."""
        # The bottle's own CA, which bottle made when it started the bottle.
        certificates = functools.partial(ca.server_context, bottle, intercepted)
        return egress.Gateway(
            bottle, self.policy, services=HostServices(git_dir, remotes=remotes), injections=injections,
            certificates=certificates,
        )

    async def bind(
        self, bottle: str, git_dir: Path | None = None, features: tuple[str, ...] = (),
        remotes: tuple[Remote, ...] = (), token: bytes | None = None,
    ) -> bytes:
        """Answer `bottle`'s flows at the decider; returns its token, a new one unless `token` is given."""
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            # Credentials are read here, each time a bottle is bound, so logging
            # in or out reaches a bottle when it next starts.
            injections = tuple(await asyncio.to_thread(auth.injections_for, features))
            intercepted = tuple(await asyncio.to_thread(auth.intercepted_hosts, features))
            gateway = self._gateway(bottle, git_dir, remotes, injections, intercepted)
            return self.decider.bind(bottle, gateway, token)

    async def release(self, bottle: str) -> None:
        """Stop answering `bottle`'s flows, and forget its token: its machine has stopped."""
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            self.decider.release(bottle)

    async def restore(self, bound: list[tuple]) -> None:
        """Answer the flows of bottles the decider gave tokens to before bottled last stopped.

        Each is (name, git dir, features, remotes), as bound_bottles() gives
        them. Their machines still hold those tokens, so the same ones are bound.
        """
        for bottle, git_dir, features, remotes in bound:
            token = self.decider.tokens.get(bottle)
            if token is None:
                continue
            try:
                await self.bind(bottle, git_dir, features, remotes, token)
            except Exception as e:  # one bad bottle mustn't stop the rest
                log.warning("%s: couldn't restore its decisions: %s", bottle, e)


def bound_bottles() -> list[tuple[str, Path | None, tuple[str, ...], tuple[Remote, ...]]]:
    """(name, repo git dir, features, remotes) of every ready bottle that has a token at the decider."""
    from bottle import bottles  # bottles imports this module

    tokens = decider_.Decider().tokens
    return [(b.name, bottles.git_dir(b), tuple(b.features), b.remotes) for b in bottles.load().values()
            if b.status == "ready" and b.name in tokens]


async def serve(path: Path | None = None, daemon: Daemon | None = None) -> None:
    path = path or socket_path()
    daemon = daemon or Daemon()
    # Owner-only: anyone who can reach the socket can bind bottles and read their tokens.
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
    try:
        decider = await daemon.decider.start(daemon.decider_port)
    except OSError as e:
        raise BottleError(f"bottled's decider can't listen on 127.0.0.1:{daemon.decider_port}: {e.strerror or e}") from None
    daemon.decider_address = "127.0.0.1:%d" % decider.sockets[0].getsockname()[1]
    log.info("decider listening on %s", daemon.decider_address)
    # Before answering anyone: whoever started bottled may be about to use a bottle whose machine
    # still holds its token, and its flows must find it bound.
    await daemon.restore(await asyncio.to_thread(bound_bottles))
    server = await asyncio.start_unix_server(daemon.handle, str(path))
    os.chmod(path, 0o600)
    log.info("bottled %d listening on %s", os.getpid(), path)
    try:
        async with server:
            await daemon.stopping.wait()
    finally:
        # Closed, not released: the decider's tokens outlive bottled, as their machines do.
        decider.close()
        path.unlink(missing_ok=True)
        log.info("bottled %d stopped", os.getpid())
