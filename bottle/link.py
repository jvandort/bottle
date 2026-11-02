"""The link: what a remote runner's bottled asks of its controller's.

A runner elsewhere runs bottled's runner half alone (daemon.Daemon). What a
bottle there needs of the controller -- what bottle.host answers, and its
credentials' flows -- it asks over the link: a Unix socket on the runner,
~/.bottle/link.sock, that the controller's ssh connection to it forwards to
a socket of the controller's own bottled, one per runner (Links).

Each connection carries one request: a JSON line, then the `length` bytes of
body it says. The answer is a JSON line, {"ok": true, ...} and its own body
the same way, or {"ok": false, "error": ...}. A credential's flow is the one
exception: after its line, the connection is the flow, both ways.

  bind     {bottle, repos, features}: the controller makes the bottle's
           side there, and answers where its credentials are for, without
           the credentials (the runner redirects those flows to it).
  release  {bottle}: its machine has stopped.
  host     {bottle, method, path, query, headers} + body: a request to
           bottle.host; answered with the whole HTTP response, as body.
  flow     {bottle, address, port, first}: a flow redirected for a
           credential, its first bytes base64; then the flow itself.

The controller knows which runner a connection came from by the socket it
came in on, and serves only bottles bound from that runner.
"""

import asyncio
import base64
import json
import logging
from pathlib import Path

from bottle import egress
from bottle.errors import BottleError
from bottle.host import HostServices, HttpRequest, Remote, Served

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


class _Captured:
    """A writer that keeps what's written: an HTTP response, to send back whole."""

    def __init__(self) -> None:
        self.data = bytearray()

    def write(self, data: bytes) -> None:
        self.data += data

    async def drain(self) -> None:
        pass


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
            raise BottleError(f"the controller: {answer.get('error', 'failed')}")
        return answer, data

    async def egress(self, bottle: str, repos: tuple[Served, ...], features: tuple[str, ...]):
        answer, _ = await self._ask({"op": "bind", "bottle": bottle, "repos": served_to_json(repos),
                                     "features": list(features)})
        injections = tuple(egress.Injection(i["host"], i["header"], "", i["port"], feature=i.get("feature", ""))
                           for i in answer["injections"])
        return injections, _LinkedServices(self, bottle), _LinkedGateway(self, bottle)

    async def release(self, bottle: str) -> None:
        try:
            await self._ask({"op": "release", "bottle": bottle})
        except BottleError as e:  # it learns when it binds the bottle again
            log.warning("%s: couldn't tell the controller it stopped: %s", bottle, e)


class _LinkedServices:
    """bottle.host for a bottle on a runner: every request answered by its controller."""

    def __init__(self, link: LinkController, bottle: str) -> None:
        self.link, self.bottle = link, bottle

    async def handle(self, request: HttpRequest, writer) -> str:
        try:
            answer, response = await self.link._ask(
                {"op": "host", "bottle": self.bottle, "method": request.method, "path": request.path,
                 "query": request.query, "headers": request.headers}, request.body)
        except (BottleError, ConnectionError, OSError) as e:
            body = f"{e}\n".encode()
            writer.write(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\n"
                         + f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body)
            await writer.drain()
            return "unreachable"
        writer.write(response)
        await writer.drain()
        return answer.get("outcome", "ok")


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

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            message, body = await _receive(reader)
        except (ConnectionError, ValueError, asyncio.IncompleteReadError):
            writer.close()
            return
        op, bottle = message.get("op"), message.get("bottle", "")
        try:
            if op == "flow":
                side = self.bottles.get(bottle)
                if side is None:
                    raise BottleError(f"no bottle {bottle!r} bound from runner {self.runner}")
                flow = egress.Flow(message["address"], message["port"], base64.b64decode(message["first"]))
                await side[1].serve_credential(flow, reader, writer)
                return
            reply, data = await self._answer(op, bottle, message, body)
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
                where, services, gateway = await self.controller.egress(
                    bottle, served_from_json(message.get("repos", [])), tuple(message.get("features", ())))
                self.bottles[bottle] = (services, gateway)
                log.info("%s: bound from runner %s", bottle, self.runner)
                return {"ok": True, "injections": [{"host": i.host, "header": i.header, "port": i.port,
                                                    "feature": i.feature} for i in where]}, b""
            case "release":
                self.bottles.pop(bottle, None)
                return {"ok": True}, b""
            case "host":
                side = self.bottles.get(bottle)
                if side is None:
                    raise BottleError(f"no bottle {bottle!r} bound from runner {self.runner}")
                captured = _Captured()
                request = HttpRequest(message["method"], message["path"], message.get("query", ""),
                                      message.get("headers", {}), body)
                outcome = await side[0].handle(request, captured)
                return {"ok": True, "outcome": outcome}, bytes(captured.data)
            case _:
                raise BottleError(f"no link operation {op!r}")
