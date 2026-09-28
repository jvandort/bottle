"""Egress proxy: a bottle's only way out.

Bottles sit on internal networks that reach nothing but the host. Each bottle
gets one of these proxies on its network's gateway. The host makes the actual
connections, so they use the host's DNS and routes (including any VPN),
and the policy is enforced where root in the bottle can't reach it.

Speaks the two things proxy-aware clients send: CONNECT (HTTPS, SSH, any TCP)
and absolute-form plain HTTP requests (apt).
"""

import asyncio
import fnmatch
import ipaddress
import logging
import socket
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from urllib.parse import urlsplit

from bottle import host

log = logging.getLogger("bottle.egress")
# Silent unless the application configures logging (the CLI does).
log.addHandler(logging.NullHandler())

MAX_HEADER_BYTES = 64 * 1024
HEADER_TIMEOUT = 30
CONNECT_TIMEOUT = 10
# Hop-by-hop headers meant for the proxy, never forwarded upstream.
PROXY_HEADERS = {"proxy-connection", "proxy-authorization", "connection", "keep-alive"}

IPAddress = ipaddress.IPv4Address | ipaddress.IPv6Address
Resolver = Callable[[str, int], Awaitable[list[str]]]


@dataclass(frozen=True)
class Policy:
    """Which destinations a bottle may reach.

    Public addresses are allowed. Private addresses (RFC 1918, ULA, CGNAT) are
    reachable only via hostnames matching `allow_private`, e.g. "*.corp.example.com".
    Loopback, link-local, multicast and unspecified addresses are never
    reachable: they are the host itself or its local network.
    """

    allow_private: tuple[str, ...] = ()

    def allows(self, host: str, ip: IPAddress) -> bool:
        if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped:
            ip = ip.ipv4_mapped
        if ip.is_loopback or ip.is_link_local or ip.is_multicast or ip.is_unspecified:
            return False
        if ip.is_global:
            return True
        return any(fnmatch.fnmatch(host.lower(), pattern.lower()) for pattern in self.allow_private)


class Refused(Exception):
    """The request can't be served; carries the HTTP status to answer with."""

    def __init__(self, status: int, reason: str, outcome: str) -> None:
        super().__init__(reason)
        self.status = status
        self.reason = reason
        self.outcome = outcome


@dataclass
class Request:
    method: str
    host: str
    port: int
    # For plain HTTP: the request head to send upstream. None for CONNECT.
    upstream_head: bytes | None
    # For plain HTTP: the path, query and headers (lowercase names), for host services.
    path: str = ""
    query: str = ""
    headers: dict[str, str] | None = None


class EgressProxy:
    def __init__(
        self, name: str, policy: Policy, resolve: Resolver | None = None, services: "host.HostServices | None" = None
    ) -> None:
        self.name = name
        self.policy = policy
        self.resolve = resolve or _resolve
        # Requests to http://bottle.host/ are answered here, never forwarded.
        self.services = services

    async def start(self, host: str, port: int) -> asyncio.Server:
        return await asyncio.start_server(self._handle, host, port, limit=MAX_HEADER_BYTES)

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        started = time.monotonic()
        request, ip, up, down = None, None, [0], [0]
        outcome = "ok"
        upstream_writer = None
        try:
            request = await self._read_request(reader)
            if request.host == host.HOST:
                ip = "host"
                outcome = await self._host(request, reader, writer)
                return
            ip, upstream_reader, upstream_writer = await self._open(request.host, request.port)
            if request.upstream_head is None:
                writer.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            else:
                upstream_writer.write(request.upstream_head)
                up[0] += len(request.upstream_head)
            await _relay(reader, writer, upstream_reader, upstream_writer, up, down)
        except Refused as e:
            outcome = e.outcome
            await _respond(writer, e.status, e.reason)
        except Exception:
            outcome = "error"
            log.exception("%s: unexpected error", self.name)
        finally:
            for w in (upstream_writer, writer):
                if w is not None:
                    w.close()
            target = f"{request.method} {request.host}:{request.port}" if request else "-"
            log.log(
                logging.INFO if outcome == "ok" else logging.WARNING,
                "%s %s -> %s %s up=%d down=%d %.2fs",
                self.name, target, ip or "-", outcome, up[0], down[0], time.monotonic() - started,
            )

    async def _host(self, request: Request, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> str:
        if self.services is None or request.upstream_head is None:  # no services, or a CONNECT
            raise Refused(403, "Forbidden: no host services here", "denied")
        try:
            body = await host.read_body(reader, request.headers)
        except (ValueError, asyncio.IncompleteReadError):
            raise Refused(400, "Bad Request", "bad-request") from None
        return await self.services.handle(
            host.HttpRequest(request.method, request.path, request.query, request.headers, body), writer
        )

    async def _read_request(self, reader: asyncio.StreamReader) -> Request:
        try:
            head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), HEADER_TIMEOUT)
        except asyncio.LimitOverrunError:
            raise Refused(431, "Request Header Fields Too Large", "bad-request") from None
        except (asyncio.IncompleteReadError, TimeoutError):
            raise Refused(400, "Bad Request", "bad-request") from None

        request_line, *header_lines = head.decode("latin-1").split("\r\n")[:-2]
        parts = request_line.split(" ")
        if len(parts) != 3 or not parts[2].startswith("HTTP/"):
            raise Refused(400, "Bad Request", "bad-request")
        method, target, version = parts

        if method == "CONNECT":
            host, port = _split_host_port(target)
            return Request(method, host, port, None)

        url = urlsplit(target)
        if url.scheme != "http" or not url.hostname:
            # HTTPS goes through CONNECT; origin-form means the client isn't proxy-aware.
            raise Refused(400, "Bad Request", "bad-request")
        path = (url.path or "/") + (f"?{url.query}" if url.query else "")
        headers = [line for line in header_lines if line.split(":", 1)[0].strip().lower() not in PROXY_HEADERS]
        if not any(line.lower().startswith("host:") for line in headers):
            headers.insert(0, f"Host: {url.netloc}")
        # One request per connection keeps relaying simple: the upstream closes when done.
        headers.append("Connection: close")
        upstream_head = "\r\n".join([f"{method} {path} {version}", *headers, "", ""]).encode("latin-1")
        parsed = {}
        for line in header_lines:
            name, _, value = line.partition(":")
            parsed[name.strip().lower()] = value.strip()
        return Request(method, url.hostname, url.port or 80, upstream_head, url.path or "/", url.query, parsed)

    async def _open(self, host: str, port: int) -> tuple[str, asyncio.StreamReader, asyncio.StreamWriter]:
        try:
            addresses = await self.resolve(host, port)
        except OSError:
            raise Refused(502, "Bad Gateway: cannot resolve host", "unresolved") from None
        # Connect to the exact addresses we checked, so a second lookup can't swap them.
        allowed = [a for a in addresses if self.policy.allows(host, ipaddress.ip_address(a))]
        if not allowed:
            raise Refused(403, "Forbidden: destination not allowed", "denied")
        for address in allowed:
            try:
                reader, writer = await asyncio.wait_for(asyncio.open_connection(address, port), CONNECT_TIMEOUT)
                return address, reader, writer
            except (OSError, TimeoutError):
                continue
        raise Refused(502, "Bad Gateway: cannot connect", "failed")


async def _resolve(host: str, port: int) -> list[str]:
    infos = await asyncio.get_running_loop().getaddrinfo(host, port, type=socket.SOCK_STREAM)
    return list(dict.fromkeys(info[4][0] for info in infos))


async def _relay(client_r, client_w, upstream_r, upstream_w, up: list[int], down: list[int]) -> None:
    """Copy both ways until the upstream is done.

    The client finishing only half-closes the upstream (it may still be
    answering); the upstream finishing ends the exchange.
    """
    sending = asyncio.create_task(_pipe(client_r, upstream_w, up))
    try:
        await _pipe(upstream_r, client_w, down)
    finally:
        sending.cancel()


async def _pipe(src: asyncio.StreamReader, dst: asyncio.StreamWriter, count: list[int]) -> None:
    try:
        while data := await src.read(64 * 1024):
            dst.write(data)
            await dst.drain()
            count[0] += len(data)
        if dst.can_write_eof():
            dst.write_eof()
    except (ConnectionError, OSError):
        pass


async def _respond(writer: asyncio.StreamWriter, status: int, reason: str) -> None:
    body = f"{reason}\n".encode()
    writer.write(
        f"HTTP/1.1 {status} {reason.split(':')[0]}\r\n"
        f"Content-Type: text/plain\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode()
        + body
    )
    try:
        await writer.drain()
    except (ConnectionError, OSError):
        pass


def _split_host_port(target: str) -> tuple[str, int]:
    host, sep, port = target.rpartition(":")
    if not sep or not port.isdigit() or not host:
        raise Refused(400, "Bad Request", "bad-request")
    return host.strip("[]"), int(port)


async def serve(name: str, host: str, port: int, policy: Policy) -> None:
    server = await EgressProxy(name, policy).start(host, port)
    log.info("%s listening on %s:%d", name, host, port)
    async with server:
        await server.serve_forever()
