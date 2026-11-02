"""The link: what a remote runner's bottled asks of its controller's.

A runner elsewhere runs bottled's runner half alone (daemon.Daemon). What a
bottle there needs of the controller -- pushes into its hubs, messages, its
credentials' flows, and its mirrors kept up to date -- it asks over the
link: a Unix socket on the runner,
~/.bottle/link.sock, that the controller's ssh connection to it forwards to
a socket of the controller's own bottled, one per runner (Links).

Each connection carries one request: a JSON line, then the `length` bytes of
body it says. The answer is a JSON line, {"ok": true, ...} and its own body
the same way, or {"ok": false, "error": ...}. A credential's flow is the one
exception: after its line, the connection is the flow, both ways.

  bind         {bottle, repos, features}: the controller makes the bottle's
               side there, and answers where its credentials are for,
               without the credentials (the runner redirects those flows).
  release      {bottle}: its machine has stopped.
  push         {bottle, method, path, query, headers} + body: a push to
               bottle.host, into the hub; answered with git http-backend's
               output, as body.
  deliver      {bottle, to, text}: a message, as host.HostServices takes it.
  peers        {bottle}: the running bottles it may see.
  flow         {bottle, address, port, first}: a flow redirected for a
               credential, its first bytes base64; then the flow itself.
  upload-pack  {repo, pull_through}: a mirror's fetch of the hub `repo` (a
               mirrors.key), origin fetched into it first if asked; then
               git's protocol, both ways (mirrors.py, remote.relay).

The controller knows which runner a connection came from by the socket it
came in on, and serves only bottles bound from that runner, and only the
hubs of bottles on it.
"""

import asyncio
import base64
import json
import logging
import os
from pathlib import Path

from bottle import cache, egress, host
from bottle.errors import BottleError
from bottle.host import UPSTREAM, HostServices, HttpRequest, Remote, Served, ServiceError

log = logging.getLogger("bottle.link")
log.addHandler(logging.NullHandler())

# The socket on a runner its controller's link reaches, under its BOTTLE_HOME.
SOCKET = "link.sock"


def served_to_json(repos: tuple[Served, ...]) -> list[dict]:
    return [{"git_dir": str(r.git_dir) if r.git_dir else None, "at": r.at,
             "remotes": [remote.spec for remote in r.remotes]} for r in repos]


def served_from_json(repos: list[dict]) -> tuple[Served, ...]:
    return tuple(Served(Path(r["git_dir"]) if r.get("git_dir") else None,
                        tuple(Remote.parse(spec) for spec in r.get("remotes", ())), r.get("at", ""))
                 for r in repos)


async def _send(writer: asyncio.StreamWriter, message: dict, body: bytes = b"") -> None:
    writer.write(json.dumps({**message, "length": len(body)}).encode() + b"\n" + body)
    await writer.drain()


async def _receive(reader: asyncio.StreamReader) -> tuple[dict, bytes]:
    line = await reader.readline()
    if not line:
        raise ConnectionError("the link closed")
    message = json.loads(line)
    return message, await reader.readexactly(message.get("length", 0))


class _Refused(BottleError):
    """What the controller refused, with the HTTP status bottle.host answers it with."""

    def __init__(self, status: int, reason: str, why: str) -> None:
        super().__init__(why)
        self.status, self.reason = status, reason


# --- the runner's end ----------------------------------------------------------------


class LinkController:
    """The controller's half of bottled, for a runner's: everything it asks crosses the link at `path`."""

    def __init__(self, path: Path) -> None:
        self.path = path

    async def _open(self) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        try:
            return await asyncio.open_unix_connection(str(self.path))
        except (FileNotFoundError, ConnectionRefusedError) as e:
            raise BottleError(f"the controller can't be reached: its link isn't up ({e.strerror or e})") from None

    async def _ask(self, message: dict, body: bytes = b"") -> tuple[dict, bytes]:
        reader, writer = await self._open()
        try:
            await _send(writer, message, body)
            answer, data = await _receive(reader)
        finally:
            writer.close()
        if not answer.get("ok"):
            if "status" in answer:
                raise _Refused(answer["status"], answer["reason"], answer["error"])
            raise BottleError(f"the controller: {answer.get('error', 'failed')}")
        return answer, data

    async def egress(self, bottle: str, repos: tuple[Served, ...], features: tuple[str, ...]):
        answer, _ = await self._ask({"op": "bind", "bottle": bottle, "repos": served_to_json(repos),
                                     "features": list(features)})
        from bottle import mirrors

        injections = tuple(egress.Injection(i["host"], i["header"], "", i["port"], feature=i.get("feature", ""))
                           for i in answer["injections"])
        caches = tuple(cache.Cached(c["host"], tuple(c.get("exclude", ()))) for c in answer.get("caches", ()))
        # Its repos as this runner has them: their mirrors.
        here = tuple(Served(mirrors.path(mirrors.key(r.git_dir)) if r.git_dir else None, r.remotes, r.at)
                     for r in repos)
        services = HostServices(here, bottle=bottle, deliver=self.deliver, peers=self.peers, git=MirrorGit(self, bottle))
        return injections, caches, services, _LinkedGateway(self, bottle)

    async def deliver(self, sender: str, to: str, text: str) -> None:
        try:
            await self._ask({"op": "deliver", "bottle": sender, "to": to, "text": text})
        except _Refused as e:
            raise ServiceError(e.status, e.reason, str(e)) from None
        except BottleError as e:
            raise ServiceError(503, "Service Unavailable", str(e)) from None

    async def peers(self, asker: str) -> list[dict]:
        try:
            answer, _ = await self._ask({"op": "peers", "bottle": asker})
        except _Refused as e:
            raise ServiceError(e.status, e.reason, str(e)) from None
        except BottleError as e:
            raise ServiceError(503, "Service Unavailable", str(e)) from None
        return answer["peers"]

    async def release(self, bottle: str) -> None:
        try:
            await self._ask({"op": "release", "bottle": bottle})
        except BottleError as e:  # it learns when it binds the bottle again
            log.warning("%s: couldn't tell the controller it stopped: %s", bottle, e)


class MirrorGit:
    """A bottle's git on a runner: fetches served from the runner's mirror of each hub, brought up to date as
    each fetch starts; pushes into the hub, through the controller."""

    def __init__(self, link: LinkController, bottle: str) -> None:
        self.link, self.bottle = link, bottle

    async def before_fetch(self, repo: Served, remote: Remote) -> None:
        from bottle import mirrors

        assert repo.git_dir is not None
        # The mirror is named for its hub: its git dir is mirrors.path(key).
        await asyncio.to_thread(mirrors.ensure, repo.git_dir.name.removesuffix(".git"), remote.fetch == UPSTREAM)

    async def fetch(self, repo: Served, remote: Remote, path_info: str, request: HttpRequest) -> bytes | None:
        assert repo.git_dir is not None
        return await host.fetch(repo.git_dir, repo, remote, path_info, request)

    async def push(self, repo: Served, remote: Remote, path_info: str, request: HttpRequest) -> bytes | None:
        _, output = await self.link._ask(
            {"op": "push", "bottle": self.bottle, "method": request.method, "path": request.path,
             "query": request.query, "headers": request.headers}, request.body)
        return output


class _LinkedGateway:
    """The controller's gateway for a bottle on a runner, as far as the runner's needs it: its credentials' flows."""

    def __init__(self, link: LinkController, bottle: str) -> None:
        self.link, self.bottle = link, bottle

    async def serve_credential(self, flow: egress.Flow, reader, writer) -> None:
        try:
            up_reader, up_writer = await self.link._open()
        except BottleError as e:
            log.warning("%s: %s", self.bottle, e)
            writer.close()
            return
        await _send(up_writer, {"op": "flow", "bottle": self.bottle, "address": flow.address, "port": flow.port,
                                "first": base64.b64encode(flow.first).decode()})
        await _pipe(reader, writer, up_reader, up_writer)


async def _pipe(reader, writer, other_reader, other_writer) -> None:
    """Carry bytes both ways until both are done."""

    async def one_way(source, sink) -> None:
        try:
            while data := await source.read(64 * 1024):
                sink.write(data)
                await sink.drain()
            if sink.can_write_eof():
                sink.write_eof()
        except (ConnectionError, OSError):
            pass

    try:
        await asyncio.gather(one_way(reader, other_writer), one_way(other_reader, writer))
    finally:
        for w in (writer, other_writer):
            w.close()


# --- the controller's end --------------------------------------------------------------


class LinkServer:
    """The controller's end of one runner's link: answers what that runner's bottled asks of `controller`."""

    def __init__(self, runner: str, controller) -> None:
        self.runner = runner
        self.controller = controller
        # What the controller made for each bottle bound from this runner: its bottle.host, and its gateway here.
        self.bottles: dict[str, tuple[HostServices, egress.Gateway]] = {}

    def bound(self, bottle: str) -> bool:
        return bottle in self.bottles

    def _side(self, bottle: str) -> tuple[HostServices, egress.Gateway]:
        side = self.bottles.get(bottle)
        if side is None:
            raise BottleError(f"no bottle {bottle!r} bound from runner {self.runner}")
        return side

    def _hubs(self) -> dict[str, Path]:
        """The hubs of the bottles on this runner, by their mirrors' names: the only ones it may fetch."""
        from bottle import bottles, mirrors

        hubs = {}
        for bottle in bottles.load().values():
            if bottle.runner == self.runner:
                for mount in bottle.mounts:
                    git_dir = bottles.git_dir(mount)
                    if git_dir is not None:
                        hubs[mirrors.key(git_dir)] = git_dir
        return hubs

    async def _upload_pack(self, message: dict, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """git upload-pack in the hub a runner's mirror fetches, its protocol carried both ways."""
        git_dir = (await asyncio.to_thread(self._hubs)).get(message.get("repo", ""))
        if git_dir is None:
            log.warning("link from %s: no hub %r of a bottle on it", self.runner, message.get("repo"))
            writer.close()
            return
        if message.get("pull_through"):
            await host.pull_through(git_dir)
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        process = await asyncio.create_subprocess_exec(
            "git", "upload-pack", "--strict", str(git_dir), env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE)
        await _pipe(reader, writer, process.stdout, process.stdin)
        await process.wait()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            message, body = await _receive(reader)
        except (ConnectionError, ValueError, asyncio.IncompleteReadError):
            writer.close()
            return
        op, bottle = message.get("op"), message.get("bottle", "")
        try:
            if op == "flow":
                side = self._side(bottle)
                flow = egress.Flow(message["address"], message["port"], base64.b64decode(message["first"]))
                await side[1].serve_credential(flow, reader, writer)
                return
            if op == "upload-pack":
                await self._upload_pack(message, reader, writer)
                return
            reply, data = await self._answer(op, bottle, message, body)
        except ServiceError as e:
            reply, data = {"ok": False, "status": e.status, "reason": e.reason, "error": str(e)}, b""
        except BottleError as e:
            reply, data = {"ok": False, "error": str(e)}, b""
        except Exception as e:
            log.exception("link from %s: %s failed", self.runner, op)
            reply, data = {"ok": False, "error": f"internal error: {e}"}, b""
        try:
            await _send(writer, reply, data)
        finally:
            writer.close()

    async def _answer(self, op: str, bottle: str, message: dict, body: bytes) -> tuple[dict, bytes]:
        match op:
            case "bind":
                where, caches, services, gateway = await self.controller.egress(
                    bottle, served_from_json(message.get("repos", [])), tuple(message.get("features", ())))
                self.bottles[bottle] = (services, gateway)
                log.info("%s: bound from runner %s", bottle, self.runner)
                return {"ok": True, "injections": [{"host": i.host, "header": i.header, "port": i.port,
                                                    "feature": i.feature} for i in where],
                        "caches": [{"host": c.host, "exclude": list(c.exclude)} for c in caches]}, b""
            case "release":
                self.bottles.pop(bottle, None)
                return {"ok": True}, b""
            case "push":
                services = self._side(bottle)[0]
                request = HttpRequest(message["method"], message["path"], message.get("query", ""),
                                      message.get("headers", {}), body)
                routed = services.route(request.path)
                if routed is None or routed[1].push is None:
                    raise ServiceError(403, "Forbidden", "read-only")
                output = await services.git.push(*routed, request)
                if output is None:
                    raise BottleError("git http-backend failed")
                return {"ok": True}, output
            case "deliver":
                self._side(bottle)
                await self.controller.deliver(bottle, message["to"], message["text"])
                return {"ok": True}, b""
            case "peers":
                self._side(bottle)
                return {"ok": True, "peers": await self.controller.peers(bottle)}, b""
            case _:
                raise BottleError(f"no link operation {op!r}")
