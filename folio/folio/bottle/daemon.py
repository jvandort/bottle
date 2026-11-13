"""folio serve (once bottled): the daemon that serves what bottles' machines route to it.

It's folio's, run as `folio serve`, its sockets and log in folio's home (home()); its code is bottle's still,
shared, until it moves with the rest of what's folio's.

It runs on the controller alone, for every runner's bottles (Daemon): a remote
runner's machines reach its sockets through ones on the runner that bottle-cli
forwards here.

bottle-cli decides a bottle's connections itself, by the policy its machine
was made with (services.network), and gives folio serve the ones that policy
routes to it, on a socket per service (services.py): bottle.host's services,
ssh to bottles, and credentials' hosts. Commands don't manage it directly:
bind() has it answer a bottle's machine, starting it if it isn't running, and
release_egress() stops that. Its control socket speaks one JSON line per request.
"""

import asyncio
import functools
import hashlib
import json
import logging
import os
import resource
import socket
import subprocess
import sys
import time
from pathlib import Path

from folio.bottle import auth, egress, messages, services as services_, specs
from folio.bottle.host import HostServices, Remote, Served, ServiceError
from folio.bottle.errors import BottleError
from folio.bottle.store import folio_home, log_path, open_log

log = logging.getLogger("bottle.daemon")
log.addHandler(logging.NullHandler())

START_TIMEOUT = 5
# How long a message between bottles waits for its bottle's session to take it,
# and how much longer folio serve gives the exec that waits, which starts a process
# in the bottle, before giving up on it.
DELIVERY_TIMEOUT = 10
DELIVERY_GRACE = 15
# The claude feature's channel, which takes an event into a bottle's session;
# it exits 75 (EX_TEMPFAIL) when no session took the event in time.
BOTTLE_EVENT = "/usr/local/bin/bottle-event"
NOT_TAKEN = 75


# macOS caps Unix socket paths at 104 bytes.
MAX_SOCKET_PATH = 100
# The open files folio serve asks for: two for each flow it carries or serves (the bottle's side and the far end's),
# and the cache's files, for every bottle's builds downloading at once. macOS gives what a shell starts 256.
OPEN_FILES = 65536


# Its log, in folio's home.
LOG = "serve"


def code_hash() -> str:
    """A hash of bottle's code: every file of this package, less what Python caches. What a command checks a
    running folio serve's against."""
    package = Path(__file__).resolve().parents[1]   # folio's, all of it
    digest = hashlib.sha256()
    for path in sorted(p for p in package.rglob("*") if p.is_file() and "__pycache__" not in p.parts):
        digest.update(str(path.relative_to(package)).encode() + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()[:16]


def home() -> Path:
    """folio's home: where folio serve keeps its sockets and its log."""
    return folio_home()


def socket_path(name: str = "serve.sock") -> Path:
    path = home() / name
    if len(str(path)) <= MAX_SOCKET_PATH:
        return path
    # A deep home: a short per-user directory instead, keyed by where it would have been.
    key = hashlib.sha256(str(path).encode()).hexdigest()[:16]
    return Path(f"/tmp/bottle-{os.getuid()}/{key}.sock")


# --- client -------------------------------------------------------------------


# What it serves, each on a socket of its own, which a machine's routes name (services.network).
SERVICES = ("host", "ssh", "credentials")


def services_sockets() -> dict[str, Path]:
    """Where folio serve answers what machines route to it, by service: bottle.host's (git, messages), ssh to
    bottles, and credentials' hosts."""
    return {name: socket_path(f"services/{name}.sock") for name in SERVICES}


def bind(bottle: str, machine: str, repos: tuple[Served, ...] = (), features: tuple[str, ...] | list[str] = ()) -> None:
    """Have folio serve answer what `machine` routes to it as `bottle`'s, starting folio serve if it isn't running.

    Its credentials are read as it's bound: a login reaches a bottle when it's
    next started, or when folio serve is.
    """
    message = {
        "op": "bind", "bottle": bottle, "machine": machine, "features": list(features),
        "repos": [{"git_dir": str(r.git_dir) if r.git_dir else None, "at": r.at,
                   "remotes": [remote.spec for remote in r.remotes]} for r in repos],
    }
    _request(message, start=True)


def release_egress(bottle: str) -> None:
    """Stop answering `bottle`'s machine, if folio serve is running at all."""
    _request({"op": "release", "bottle": bottle}, start=False)


def start() -> bool:
    """Start the daemon in the background. False if it was already running, on this code."""
    try:
        _send({"op": "ping"})
    except (FileNotFoundError, ConnectionRefusedError):
        _start_daemon()
        return True
    return _ensure_current()


def stop() -> bool:
    """Stop the daemon and every egress it serves. False if it wasn't running."""
    try:
        reply = _send({"op": "shutdown"})
        log.info("asked folio serve to stop")
    except (FileNotFoundError, ConnectionRefusedError):
        return False
    if not reply.get("ok"):
        raise BottleError(f"folio serve: {reply.get('error', 'shutdown failed')}")
    return True


# Whether this process has seen folio serve running its own code: asked once a command.
_current = False


def _ensure_current() -> bool:
    """folio serve running this code, if it's running: one running other code is restarted first.

    Other code is another checkout's, or this one's before it changed: one that
    may not know what this asks of it, or answer it as this would. Running
    bottles keep their tokens, and the new folio serve binds them again as it
    starts, so they lose only the connections they open in between. Returns
    whether it restarted it.
    """
    global _current
    if _current:
        return False
    try:
        reply = _send({"op": "ping"})
    except (FileNotFoundError, ConnectionRefusedError):
        return False  # not running: whoever starts it starts this code
    except _Closed:
        _wait_gone()  # on its way out: whoever starts it next starts this code
        return False
    _current = True
    if reply.get("code") == code_hash():
        return False
    log.info("folio serve is running other code (%s); restarting it", reply.get("code", "from before it said"))
    print("bottle: folio serve was running other code; restarting it", file=sys.stderr)
    _restart()
    return True


def _restart() -> None:
    stop()
    _wait_gone()
    _start_daemon()


def _wait_gone() -> None:
    """Until the folio serve that's stopping is gone, or would the new one find it and leave. Until then it
    answers, or, shutting down, takes a connection and closes it unanswered."""
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        try:
            _send({"op": "ping"})
        except (FileNotFoundError, ConnectionRefusedError):
            return
        except _Closed:
            pass
        time.sleep(0.05)


def _request(message: dict, start: bool, timeout: float = 30) -> dict:
    _ensure_current()
    try:
        try:
            reply = _send(message, timeout)
        except (FileNotFoundError, ConnectionRefusedError):
            if not start:
                return {"ok": True}
            _start_daemon()
            reply = _send(message, timeout)
    except _Closed:
        raise BottleError(f"folio serve closed the connection without answering {message['op']}: "
                          f"is it stopping? See {log_path(LOG, home())}") from None
    if not reply.get("ok"):
        raise BottleError(f"folio serve: {reply.get('error', 'request failed')}")
    return reply


class _Closed(ConnectionError):
    """folio serve took the connection and closed it unanswered: what one shutting down does."""


def _send(message: dict, timeout: float = 30) -> dict:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        s.connect(str(socket_path()))
        try:
            s.sendall(json.dumps(message).encode() + b"\n")
            line = s.makefile().readline()
        except (BrokenPipeError, ConnectionResetError):
            raise _Closed from None
        if not line:
            raise _Closed
        return json.loads(line)


def _start_daemon() -> None:
    log.info("starting folio serve")
    # This folio, on this Python: its package found where this one was.
    command = [sys.executable, "-m", "folio"]
    env = {**os.environ, "PYTHONPATH": os.pathsep.join(filter(None, [
        str(Path(__file__).resolve().parents[2]), os.environ.get("PYTHONPATH")]))}
    # Its own records go to the log through logging, stamped; this catches anything else it prints.
    with os.fdopen(open_log(LOG, home()), "a") as out:
        # A new session detaches it from this terminal, so it outlives the command. It runs in /, not the
        # command's directory: that may be deleted while it runs (a machine's), and then every git it runs fails.
        subprocess.Popen(
            [*command, "serve", "start", "--foreground"],
            stdin=subprocess.DEVNULL, stdout=out, stderr=out, start_new_session=True, cwd="/", env=env,
        )
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        try:
            _send({"op": "ping"})
            return
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.05)
    raise BottleError(f"folio serve didn't start; see {log_path(LOG, home())}")


# --- server -------------------------------------------------------------------


class Daemon:
    """folio serve: the services sockets, each bottle's gateway, and what those hand it: ssh to bottles, and
    messages between them."""

    def __init__(self, services: services_.Services | None = None,
                 services_paths: dict[str, Path] | None = None) -> None:
        self.services = services or services_.Services()
        self.services_paths = services_paths
        self.stopping = asyncio.Event()
        # Serializes work per bottle: startup restore and requests may overlap.
        self.locks: dict[str, asyncio.Lock] = {}
        # The code it runs, by its hash, as it was when it started: what a command checks it against.
        self.code = code_hash()

    def running(self, bottle: str) -> bool:
        """Whether `bottle` is bound here: its machine is running, as far as folio serve knows."""
        return self.services.bound(bottle)

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        line = await reader.readline()
        if not line:  # a liveness probe: connected and hung up
            writer.close()
            return
        try:
            message = json.loads(line)
            match message.get("op"):
                case "ping":
                    reply = {"ok": True, "code": self.code}
                case "bind":
                    repos = tuple(Served(Path(r["git_dir"]) if r.get("git_dir") else None,
                                         tuple(Remote.parse(spec) for spec in r.get("remotes", ())), r.get("at", ""))
                                  for r in message.get("repos", ()))
                    await self.bind(message["bottle"], message["machine"], repos, tuple(message.get("features", ())))
                    reply = {"ok": True}
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

    async def bind(self, bottle: str, machine: str, repos: tuple[Served, ...] = (), features: tuple[str, ...] = ()) -> None:
        """Answer what `machine` routes here as `bottle`'s.

        Its credentials are read now, each time it's bound, so a credential set
        or removed reaches a bottle when it next starts, or folio serve does.
        Only for the hosts its machine routes here, which it was made with:
        those its features name since then go direct until it's made again.
        """
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            injections = tuple(await asyncio.to_thread(auth.injections_for, features))
            services = HostServices(repos, bottle=bottle, deliver=self.deliver, peers=self.peers)
            gateway = egress.Gateway(bottle, services, injections, ssh=functools.partial(self.ssh, bottle))
            self.services.bind(bottle, machine, gateway)

    async def release(self, bottle: str) -> None:
        """Stop answering `bottle`'s machine: it's gone (deleted, or its thread paused). With none left to answer,
        folio serve stops: the next bottle made starts it again."""
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            self.services.release(bottle)
        if not self.services.bindings:
            log.info("no bottle left to answer: stopping")
            self.stopping.set()

    async def restore(self, bound: list[tuple]) -> list[tuple]:
        """Answer the machines of bottles there were before folio serve started.

        Each is (name, machine, the repos it's served, features), as
        bound_bottles() gives them. Returns those it couldn't bind.
        """
        unrestored = []
        for binding in bound:
            bottle, machine, repos, features = binding
            if self.services.bound(bottle):
                continue
            try:
                await self.bind(bottle, machine, repos, features)
            except Exception as e:  # one bad bottle mustn't stop the rest
                log.warning("%s: couldn't restore its decisions: %s", bottle, e)
                unrestored.append(binding)
        return unrestored

    async def ssh(self, bottle: str, address: str) -> list[str]:
        """The command line that runs sshd -i in the bottle `bottle` may ssh to at `address`, wherever that runs;
        refused if it may ssh to none there, or that one isn't running."""
        from folio.bottle import bottles  # bottles imports this module

        try:
            return await asyncio.to_thread(bottles.sshd_command, bottle, address)
        except BottleError as e:
            log.warning("%s: no ssh at %s: %s", bottle, address, e)
            raise egress.Refused(None, str(e), "no-ssh") from None

    async def deliver(self, sender: str, to: str, text: str) -> None:
        """Hand `text` to the bottle `to` reaches from `sender`, as an event from `sender`; raises ServiceError.

        `to` is an address issued to `sender` (see messages.py), or a bottle's
        name if `sender` may message any. The event's from is `sender`'s address,
        issued to the recipient, so it can answer. bottle-event send, run in the
        recipient, queues the event and waits until the session takes it, so
        this returns once it has.
        """
        from folio.bottle import bottles  # bottles imports this module

        registry = await asyncio.to_thread(bottles.load)
        if messages.is_address(to):
            target = await asyncio.to_thread(messages.target, to, sender)
        else:
            target = to if "message-any" in _permissions(registry.get(sender)) else None
        if target == sender:
            raise ServiceError(400, "Bad Request", "a bottle can't send a message to itself")
        bottle = registry.get(target) if target else None
        if bottle is None or not self.running(target):
            # The same whether there's no such bottle or the sender may not reach it: a refusal gives nothing away.
            raise ServiceError(404, "Not Found", f"no running bottle you can reach as {to}")
        reply_to = await asyncio.to_thread(messages.address, sender, target)
        try:
            # A runner elsewhere is asked for it, over ssh.
            command = await asyncio.to_thread(bottles.runner_of(bottle).exec_command,
                bottle.machine, [BOTTLE_EVENT, "send", "-t", str(DELIVERY_TIMEOUT), "-f", reply_to], user=bottles.USER)
        except BottleError as e:
            raise ServiceError(502, "Bad Gateway", f"couldn't deliver to {to}: {e}") from None
        process = await asyncio.create_subprocess_exec(
            *command, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            _, err = await asyncio.wait_for(process.communicate(text.encode()), DELIVERY_TIMEOUT + DELIVERY_GRACE)
        except TimeoutError:
            process.kill()
            await process.wait()
            raise ServiceError(504, "Gateway Timeout", f"couldn't reach {to} to deliver the message") from None
        if process.returncode == NOT_TAKEN:
            raise ServiceError(504, "Gateway Timeout", f"{to} has no session that took the message within {DELIVERY_TIMEOUT}s")
        if process.returncode != 0:
            why = err.decode(errors="replace").strip() or f"exit {process.returncode}"
            raise ServiceError(502, "Bad Gateway", f"couldn't deliver to {to}: {why}")
        log.info("%s: delivered a message from %s", target, sender)

    async def peers(self, asker: str) -> list[dict]:
        """The running bottles but `asker`, for a bottle with list-bottles: each one's name, repo and labels."""
        from folio.bottle import bottles  # bottles imports this module

        registry = await asyncio.to_thread(bottles.load)
        if "list-bottles" not in _permissions(registry.get(asker)):
            raise ServiceError(403, "Forbidden", "listing bottles needs the list-bottles permission")
        return [{"name": name, "repo": Path(bottle.mounts[0].repo).name if bottle.mounts else None,
                 "labels": specs.labels(bottle.spec)}
                for name, bottle in sorted(registry.items()) if name != asker and self.running(name)]


def _permissions(bottle) -> list[str]:
    """What a bottle's spec permits it, by bottle's own record of it: never what the bottle says."""
    return bottle.spec.get("permissions", []) if bottle is not None else []


def bound_bottles() -> list[tuple[str, str, tuple[Served, ...], tuple[str, ...]]]:
    """(name, machine, the repos it's served, features) of every ready bottle, wherever it runs, but on a runner
    detached (turned off)."""
    from folio.bottle import bottles, runners  # bottles imports this module

    return [(b.name, b.machine, bottles.served(b), tuple(b.features)) for b in bottles.load().values()
            if b.status == "ready" and (b.runner == runners.LOCAL or not runners.is_detached(b.runner))]


def raise_open_files(wanted: int = OPEN_FILES) -> int:
    """Raise this process's limit on open files towards `wanted`, as far as the system lets it; the limit it has.

    Run out, every flow fails at once: a socket, a pipe to openssl, even a
    lookup (getaddrinfo's socket) fails with EMFILE, and the bottles see
    resets and failed lookups that look like a flaky network.
    """
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    target = wanted if hard == resource.RLIM_INFINITY else min(wanted, hard)
    # macOS refuses more than kern.maxfilesperproc, whatever the hard limit says: halve until it's taken.
    while target > soft:
        try:
            resource.setrlimit(resource.RLIMIT_NOFILE, (target, hard))
            return target
        except (ValueError, OSError):
            target //= 2
    return soft


async def serve(path: Path | None = None, daemon: Daemon | None = None) -> None:
    path = path or socket_path()
    log.info("open files: up to %d", raise_open_files())
    daemon = daemon or Daemon()
    # Owner-only: anyone who can reach the socket can bind bottles.
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.parent.stat().st_uid != os.getuid():
        raise BottleError(f"{path.parent} is owned by another user")
    if path.exists():
        try:
            _, writer = await asyncio.open_unix_connection(str(path))
            writer.close()
            log.info("another folio serve is already running; exiting")
            return
        except (ConnectionRefusedError, FileNotFoundError):
            path.unlink()  # left over from a daemon that died
    services_paths = daemon.services_paths or services_sockets()
    routed = [await daemon.services.start(p) for p in services_paths.values()]
    log.info("services listening on %s", ", ".join(str(p) for p in services_paths.values()))
    # Before answering anyone: whoever started it may be about to use a bottle whose machine is running, and what
    # it routes here must find it bound.
    await daemon.restore(await asyncio.to_thread(bound_bottles))
    server = await asyncio.start_unix_server(daemon.handle, str(path))
    os.chmod(path, 0o600)
    log.info("folio serve %d listening on %s", os.getpid(), path)
    try:
        async with server:
            await daemon.stopping.wait()
    finally:
        for server, p in zip(routed, services_paths.values()):
            server.close()
            p.unlink(missing_ok=True)
        path.unlink(missing_ok=True)
        log.info("folio serve %d stopped", os.getpid())
