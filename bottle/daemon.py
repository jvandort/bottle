"""bottled: the daemon that decides every bottle's connections.

It's two halves in one process. The runner's (Daemon) is what has to sit
next to the machines: the decider, each bottle's gateway, and the HTTP cache.
The controller's (Controller) is what stays with the user: credentials,
pushes into hubs, and messages between bottles. A runner elsewhere runs the
first half alone, and reaches the controller's over its link; here the two
call each other in-process.

It runs the decider (see decider.py) on the runner's loopback, which smolvm asks
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
from collections.abc import Callable
from pathlib import Path

from bottle import auth, ca, cache, decider as decider_, egress, link, messages, ssh
from bottle.host import HostServices, HubGit, Remote, Served, ServiceError
from bottle.errors import BottleError
from bottle.store import bottle_home, log_path, open_log, read_json, write_json

log = logging.getLogger("bottle.daemon")
log.addHandler(logging.NullHandler())

START_TIMEOUT = 5
# How often the HTTP cache's unused files are removed, after once at start.
CACHE_EXPIRY_INTERVAL = 6 * 60 * 60
# How long a message between bottles waits for its bottle's session to take it,
# and how much longer bottled gives the exec that waits, which starts a process
# in the bottle, before giving up on it.
DELIVERY_TIMEOUT = 10
DELIVERY_GRACE = 15
# The claude feature's channel, which takes an event into a bottle's session;
# it exits 75 (EX_TEMPFAIL) when no session took the event in time.
BOTTLE_EVENT = "/usr/local/bin/bottle-event"
NOT_TAKEN = 75


# macOS caps Unix socket paths at 104 bytes.
MAX_SOCKET_PATH = 100
# How long a link to a remote runner may take to come up, in seconds; and the longest wait before trying again.
LINK_TIMEOUT = 20
LINK_BACKOFF = 60
# A link no bottle is bound through for this long is let go, checked this often, in seconds.
LINK_IDLE = 120
LINK_IDLE_CHECK = 15
# How often a runner retries binding the bottles it couldn't when its link was down.
RESTORE_INTERVAL = 5


def socket_path(name: str = "bottled.sock") -> Path:
    path = bottle_home() / name
    if len(str(path)) <= MAX_SOCKET_PATH:
        return path
    # A deep BOTTLE_HOME: use a short per-user directory, keyed by BOTTLE_HOME.
    key = hashlib.sha256(str(bottle_home() / name).encode()).hexdigest()[:16]
    return Path(f"/tmp/bottle-{os.getuid()}/{key}.sock")


# --- client -------------------------------------------------------------------


def bind_decider(
    bottle: str, repos: tuple[Served, ...] = (), features: tuple[str, ...] | list[str] = (),
) -> tuple[str, str]:
    """Have the decider answer `bottle`'s flows; return its address and the bottle's new token, in hex.

    The token goes to the driver that starts the bottle's machine, and nowhere
    else: whoever holds it can speak for the bottle.
    """
    message = {
        "op": "bind", "bottle": bottle, "features": list(features),
        "repos": [{"git_dir": str(r.git_dir) if r.git_dir else None, "at": r.at,
                   "remotes": [remote.spec for remote in r.remotes]} for r in repos],
    }
    reply = _request(message, start=True)
    return reply["decider"], reply["token"]


def ensure_link(runner: str) -> None:
    """The link to a remote runner up: its ssh connection forwarding the runner's link.sock to bottled here."""
    _request({"op": "link", "runner": runner}, start=True, timeout=LINK_TIMEOUT + 5)


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


def _request(message: dict, start: bool, timeout: float = 30) -> dict:
    try:
        reply = _send(message, timeout)
    except (FileNotFoundError, ConnectionRefusedError):
        if not start:
            return {"ok": True}
        _start_daemon()
        reply = _send(message, timeout)
    if not reply.get("ok"):
        raise BottleError(f"bottled: {reply.get('error', 'request failed')}")
    return reply


def _send(message: dict, timeout: float = 30) -> dict:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        s.connect(str(socket_path()))
        s.sendall(json.dumps(message).encode() + b"\n")
        return json.loads(s.makefile().readline())


def _start_daemon() -> None:
    log.info("starting bottled")
    code = Path(__file__).resolve().parent.parent
    launcher = code / "bin" / "bottle"
    # A runner elsewhere has the package alone, run as bottle.remote runs it.
    command = [sys.executable, str(launcher)] if launcher.exists() else [
        sys.executable, "-I", "-c", "import sys; sys.path.insert(0, sys.argv.pop(1)); from bottle.cli import main; "
        "sys.exit(main())", str(code)]
    # Its own records go to the log through logging, stamped; this catches anything else it prints.
    with os.fdopen(open_log("bottled"), "a") as out:
        # A new session detaches it from this terminal, so it outlives the command.
        subprocess.Popen(
            [*command, "daemon", "start", "--foreground"],
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


class Controller:
    """bottled's controller half: what a bottle's gateway hands to the controller, wherever the bottle runs.

    Credentials and the certificates they're attached behind, pushes into
    the hub (HubGit), and messages between bottles. `running` says whether a
    bottle's machine is running: its runner's to know.
    """

    def __init__(self, policy: egress.Policy, running: Callable[[str], bool]) -> None:
        self.policy = policy
        self._running = running
        # Its links to remote runners, which bottles there are bound through.
        self.links = Links(self)

    def running(self, bottle: str) -> bool:
        return self._running(bottle) or any(server.bound(bottle) for server in self.links.servers.values())

    async def release(self, bottle: str) -> None:
        """A bottle here has stopped: nothing to do, since its side here is made again each time it's bound."""

    async def egress(
        self, bottle: str, repos: tuple[Served, ...], features: tuple[str, ...],
    ) -> tuple[tuple[egress.Injection, ...], HostServices, egress.Gateway]:
        """What a runner needs to bind `bottle`: where its credentials are for, its bottle.host, and its
        gateway here, which serves its credentials' flows.

        Credentials are read here, each time a bottle is bound, so logging
        in or out reaches a bottle when it next starts. Only for the hosts the
        bottle's CA covers, which it was made for when the bottle started:
        those its features name since then can't be terminated, and go
        direct until it starts again.
        """
        intercepted = tuple(await asyncio.to_thread(ca.hosts, bottle))
        injections = tuple(i for i in await asyncio.to_thread(auth.injections_for, features) if i.host.lower() in intercepted)
        certificates = functools.partial(ca.server_context, bottle, intercepted)
        gateway = egress.Gateway(bottle, self.policy, injections=injections, certificates=certificates)
        services = HostServices(repos, bottle=bottle, deliver=self.deliver, peers=self.peers, git=HubGit())
        # Where each credential is for, and nothing of the credential itself.
        where = tuple(egress.Injection(i.host, i.header, "", i.port, feature=i.feature) for i in injections)
        return where, services, gateway

    async def deliver(self, sender: str, to: str, text: str) -> None:
        """Hand `text` to the bottle `to` reaches from `sender`, as an event from `sender`; raises ServiceError.

        `to` is an address issued to `sender` (see messages.py), or a bottle's
        name if `sender` may message any. The event's from is `sender`'s address,
        issued to the recipient, so it can answer. bottle-event send, run in the
        recipient, queues the event and waits until the session takes it, so
        this returns once it has.
        """
        from bottle import bottles  # bottles imports this module

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
        """The running bottles but `asker`, for a bottle with list-bottles: each one's name, repo and thread."""
        from bottle import bottles  # bottles imports this module

        registry = await asyncio.to_thread(bottles.load)
        if "list-bottles" not in _permissions(registry.get(asker)):
            raise ServiceError(403, "Forbidden", "listing bottles needs the list-bottles permission")
        return [{"name": name, "repo": Path(bottle.mounts[0].repo).name if bottle.mounts else None,
                 "thread": bottle.spec.get("thread")}
                for name, bottle in sorted(registry.items()) if name != asker and self.running(name)]


class Links:
    """The controller's links to its remote runners (see link.py), each made when it's first needed.

    For each runner: a socket here that the runner's bottled reaches through
    the link, and an ssh connection forwarding the runner's link.sock to it,
    started again whenever it ends, waiting longer each time it fails, up to
    LINK_BACKOFF. A link that's down is only that: what the runner's bottles
    ask of the controller fails meanwhile.
    """

    def __init__(self, controller: "Controller") -> None:
        self.controller = controller
        self.servers: dict[str, link.LinkServer] = {}
        self.sockets: dict[str, asyncio.Server] = {}
        self.keeping: dict[str, asyncio.Task] = {}
        self.up: dict[str, asyncio.Event] = {}
        self.processes: dict[str, asyncio.subprocess.Process] = {}

    async def ensure(self, runner: str) -> None:
        """The link to `runner` up, or BottleError saying why it isn't."""
        from bottle import runners

        destination = runners.registered().get(runner, {}).get("ssh")
        if destination is None:
            raise BottleError(f"no runner named {runner!r}")
        if runner not in self.servers:
            server = self.servers[runner] = link.LinkServer(runner, self.controller)
            path = socket_path(f"links/{runner}.sock")
            path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            path.unlink(missing_ok=True)
            self.sockets[runner] = await asyncio.start_unix_server(server.handle, str(path))
            os.chmod(path, 0o600)
        up = self.up.setdefault(runner, asyncio.Event())
        failure: list[str] = []
        task = self.keeping.get(runner)
        if task is None or task.done():
            self.keeping[runner] = asyncio.create_task(self._keep(runner, destination, failure))
        try:
            await asyncio.wait_for(up.wait(), LINK_TIMEOUT)
        except TimeoutError:
            raise BottleError(f"runner {runner} is unreachable: its link didn't come up"
                              f"{' (' + failure[-1] + ')' if failure else ''}") from None

    async def _keep(self, runner: str, destination: str, failure: list[str]) -> None:
        """Keep the link to `runner` up while any bottle there is bound through it; then let it go."""
        transport = ssh.SshTransport(runner, destination)
        here = socket_path(f"links/{runner}.sock")
        backoff, idle = 1, 0.0
        while idle < LINK_IDLE:
            try:
                there = await asyncio.to_thread(transport.prepare_link)
                process = self.processes[runner] = await asyncio.create_subprocess_exec(
                    *transport.forward_command(there, here), stdin=asyncio.subprocess.DEVNULL,
                    stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE)
                if await asyncio.to_thread(transport.wait_for_link, there, process):
                    log.info("link to runner %s up", runner)
                    self.up[runner].set()
                    backoff = 1
                while process.returncode is None and idle < LINK_IDLE:
                    try:
                        await asyncio.wait_for(asyncio.shield(process.wait()), LINK_IDLE_CHECK)
                    except TimeoutError:
                        idle = 0.0 if self.servers[runner].bottles else idle + LINK_IDLE_CHECK
                if process.returncode is None:
                    log.info("link to runner %s let go: no bottle there is bound through it", runner)
                    process.terminate()
                    await process.wait()
                    break
                why = (await process.stderr.read()).decode(errors="replace").strip().splitlines()
                failure.append(why[-1] if why else f"ssh exited {process.returncode}")
                log.warning("link to runner %s down: %s", runner, failure[-1])
            except BottleError as e:
                failure.append(str(e))
                log.warning("link to runner %s: %s", runner, e)
            self.up[runner].clear()
            await asyncio.sleep(backoff)
            # Not tried for ever for nothing: with no bottle bound through it, the waiting counts as idle.
            idle = idle + backoff if not self.servers[runner].bottles else 0.0
            backoff = min(backoff * 2, LINK_BACKOFF)
        self.up[runner].clear()

    async def bring_up(self) -> None:
        """The links to runners with bottles: theirs may be running, and need the controller to be decided."""
        from bottle import bottles

        wanted = {b.runner for b in await asyncio.to_thread(lambda: list(bottles.load().values()))
                  if b.status == "ready" and b.runner != "local"}
        for runner in sorted(wanted):
            try:
                await self.ensure(runner)
            except BottleError as e:
                log.warning("%s", e)

    async def close(self) -> None:
        for task in self.keeping.values():
            task.cancel()
        for process in self.processes.values():
            if process.returncode is None:
                process.terminate()
        for server in self.sockets.values():
            server.close()


class Daemon:
    """bottled's runner half: the decider, and each bottle's gateway, which hands the controller what's its."""

    def __init__(
        self, policy: egress.Policy | None = None, decider: decider_.Decider | None = None,
        decider_port: int | None = None, controller: Controller | None = None,
    ) -> None:
        self.policy = policy or egress.Policy()
        self.decider = decider or decider_.Decider()
        self.decider_port = decider_.port() if decider_port is None else decider_port
        # Where the decider listens once it does: host:port.
        self.decider_address: str | None = None
        self.stopping = asyncio.Event()
        # Serializes work per bottle: startup restore and requests may overlap.
        self.locks: dict[str, asyncio.Lock] = {}
        # Every bottle's cached downloads, in one place: what one fetched, the next is answered with.
        self.store = cache.Store()
        # The controller's half: here, in this process, unless the controller is elsewhere.
        self.controller = controller or Controller(self.policy, self.decider.bound)
        # With the controller elsewhere, this runner has no registry of bottles to restore them from: what each
        # was bound with is kept here instead (remembered).
        self.remembered: Path | None = None

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
                    repos = tuple(Served(Path(r["git_dir"]) if r.get("git_dir") else None,
                                         tuple(Remote.parse(spec) for spec in r.get("remotes", ())), r.get("at", ""))
                                  for r in message.get("repos", ()))
                    token = await self.bind(message["bottle"], repos, tuple(message.get("features", ())))
                    reply = {"ok": True, "decider": self.decider_address, "token": token.hex()}
                case "release":
                    await self.release(message["bottle"])
                    reply = {"ok": True}
                case "link":
                    if not isinstance(self.controller, Controller):
                        raise BottleError("a runner has no links of its own: its controller does")
                    await self.controller.links.ensure(message["runner"])
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

    async def bind(
        self, bottle: str, repos: tuple[Served, ...] = (), features: tuple[str, ...] = (),
        token: bytes | None = None,
    ) -> bytes:
        """Answer `bottle`'s flows at the decider; returns its token, a new one unless `token` is given."""
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            injections, services, controller_gateway = await self.controller.egress(bottle, repos, features)
            intercepted = tuple(await asyncio.to_thread(ca.hosts, bottle))
            caches = tuple(c for c in await asyncio.to_thread(cache.cached_for, features) if c.host in intercepted)
            # The bottle's own CA, which bottle made when it started the bottle: what its cached hosts are answered behind.
            certificates = functools.partial(ca.server_context, bottle, intercepted)
            gateway = egress.Gateway(
                bottle, self.policy, services=services, injections=injections, certificates=certificates,
                caches=caches, store=self.store, hand_off=controller_gateway.serve_credential,
            )
            if self.remembered is not None:
                self._remember(bottle, (bottle, link.served_to_json(repos), list(features)))
            return self.decider.bind(bottle, gateway, token)

    def _remember(self, bottle: str, binding: tuple | None) -> None:
        bindings = read_json(self.remembered) or {}
        if binding is None:
            bindings.pop(bottle, None)
        else:
            bindings[bottle] = binding
        write_json(self.remembered, bindings)

    def remembered_bindings(self) -> list[tuple]:
        """What each bottle was last bound with here, as restore() takes them."""
        return [(b, link.served_from_json(repos), tuple(features))
                for b, repos, features in (read_json(self.remembered) or {}).values()]

    async def release(self, bottle: str) -> None:
        """Stop answering `bottle`'s flows, and forget its token: its machine has stopped."""
        async with self.locks.setdefault(bottle, asyncio.Lock()):
            self.decider.release(bottle)
            if self.remembered is not None:
                self._remember(bottle, None)
        await self.controller.release(bottle)

    async def restore(self, bound: list[tuple]) -> list[tuple]:
        """Answer the flows of bottles the decider gave tokens to before bottled last stopped.

        Each is (name, the repos it's served, features), as bound_bottles()
        gives them. Their machines still hold those tokens, so the same ones are bound.
        Returns those it couldn't bind.
        """
        unrestored = []
        for bottle, repos, features in bound:
            token = self.decider.tokens.get(bottle)
            if token is None or self.decider.bound(bottle):
                continue
            try:
                await self.bind(bottle, repos, features, token)
            except Exception as e:  # one bad bottle mustn't stop the rest
                log.warning("%s: couldn't restore its decisions: %s", bottle, e)
                unrestored.append((bottle, repos, features))
        return unrestored


def _permissions(bottle) -> list[str]:
    """What a bottle's spec permits it, by bottle's own record of it: never what the bottle says."""
    return bottle.spec.get("permissions", []) if bottle is not None else []


def bound_bottles() -> list[tuple[str, tuple[Served, ...], tuple[str, ...]]]:
    """(name, the repos it's served, features) of every ready bottle that has a token at the decider."""
    from bottle import bottles  # bottles imports this module

    tokens = decider_.Decider().tokens
    return [(b.name, bottles.served(b), tuple(b.features)) for b in bottles.load().values()
            if b.status == "ready" and b.name in tokens]


async def _keep_restoring(daemon: Daemon, unrestored: list[tuple]) -> None:
    while unrestored:
        await asyncio.sleep(RESTORE_INTERVAL)
        unrestored = await daemon.restore(unrestored)


async def _expire_cache(store: cache.Store) -> None:
    """Remove the cache's unused files now, and every CACHE_EXPIRY_INTERVAL after."""
    while True:
        try:
            await asyncio.to_thread(store.expire)
        except Exception:
            log.exception("couldn't expire the HTTP cache")
        await asyncio.sleep(CACHE_EXPIRY_INTERVAL)


async def serve(path: Path | None = None, daemon: Daemon | None = None) -> None:
    path = path or socket_path()
    if daemon is None:
        # On a runner elsewhere, bottle.remote names its link: the controller's half is reached through it.
        there = os.environ.get("BOTTLE_LINK")
        daemon = Daemon(controller=link.LinkController(Path(there))) if there else Daemon()
        if there:
            daemon.remembered = bottle_home() / "bindings.json"
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
    if daemon.remembered is None:
        await daemon.restore(await asyncio.to_thread(bound_bottles))
        # In the background: a runner that's off mustn't keep bottled from answering.
        retrying = asyncio.create_task(daemon.controller.links.bring_up())
    else:
        # The link may not be up yet: those that can't be bound now are tried again until they are.
        retrying = asyncio.create_task(_keep_restoring(daemon, await daemon.restore(daemon.remembered_bindings())))
    expiring = asyncio.create_task(_expire_cache(daemon.store))
    server = await asyncio.start_unix_server(daemon.handle, str(path))
    os.chmod(path, 0o600)
    log.info("bottled %d listening on %s", os.getpid(), path)
    try:
        async with server:
            await daemon.stopping.wait()
    finally:
        expiring.cancel()
        if retrying is not None:
            retrying.cancel()
        if isinstance(daemon.controller, Controller):
            await daemon.controller.links.close()
        # Closed, not released: the decider's tokens outlive bottled, as their machines do.
        decider.close()
        path.unlink(missing_ok=True)
        log.info("bottled %d stopped", os.getpid())
