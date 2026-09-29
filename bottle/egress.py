"""Egress proxy: a bottle's only way out.

Bottles sit on internal networks that reach nothing but the host. Each bottle
gets one of these proxies on its network's gateway. The host makes the actual
connections, so they use the host's DNS and routes (including any VPN),
and the policy is enforced where root in the bottle can't reach it.

Speaks the two things proxy-aware clients send: CONNECT (HTTPS, SSH, any TCP)
and absolute-form plain HTTP requests (apt).

It also holds credentials the bottle never sees (Injection): for the hosts a
feature names, the proxy attaches the real token as a request header and makes
the HTTPS connection itself.
"""

import asyncio
import fnmatch
import functools
import ipaddress
import logging
import socket
import ssl
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

    def allows(self, host: str, ip: IPAddress, private: bool = False) -> bool:
        """Whether the bottle may reach `host` at `ip`.

        `private` is for a destination a credential names (see Injection): the
        host was configured on the machine, so a private address is expected. The
        host's own addresses are still refused.
        """
        if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped:
            ip = ip.ipv4_mapped
        if ip.is_loopback or ip.is_link_local or ip.is_multicast or ip.is_unspecified:
            return False
        if ip.is_global:
            return True
        return private or any(fnmatch.fnmatch(host.lower(), pattern.lower()) for pattern in self.allow_private)


@dataclass(frozen=True)
class Injection:
    """A credential the host attaches on the bottle's behalf, for one host.

    A feature declares which hosts its credential belongs to (see
    features.py); bottled looks the credential up on the machine and builds these
    per bottle, so the token itself stays outside the bottle.

    The bottle addresses an injected host over plain HTTP, because a CONNECT
    tunnel is opaque and a header can only be attached to a request the proxy
    can read. The proxy always reaches the server over TLS, so a credential
    never travels in the clear beyond the bottle's own network: that hop is
    between the bottle and its gateway on the machine, and the request that leaves
    the machine is the ordinary HTTPS one the server expects.

    An injected host is also reachable where the policy would otherwise refuse
    a private address, since naming the host is what configuring the
    credential means. Nothing else about that host opens up: a CONNECT to it
    is judged by the policy alone.
    """

    host: str  # a pattern, e.g. "teamcity.corp.example.com" or "*.example.com"
    header: str
    value: str  # the whole header value, credential included
    port: int = 443

    def matches(self, host: str) -> bool:
        return fnmatch.fnmatch(host.lower(), self.host.lower())


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
    # Set when a credential was attached: the proxy makes this connection
    # over TLS, and allows a private address for this destination.
    injected: bool = False


class EgressProxy:
    def __init__(
        self,
        name: str,
        policy: Policy,
        resolve: Resolver | None = None,
        services: "host.HostServices | None" = None,
        injections: tuple[Injection, ...] = (),
    ) -> None:
        self.name = name
        self.policy = policy
        self.resolve = resolve or _resolve
        # Requests to http://bottle.host/ are answered here, never forwarded.
        self.services = services
        # Credentials the host attaches for the bottle, by destination.
        self.injections = tuple(injections)

    async def start(self, host: str, port: int) -> asyncio.Server:
        return await asyncio.start_server(self._handle, host, port, limit=MAX_HEADER_BYTES)

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        started = time.monotonic()
        request, ip, up, down = None, None, [0], [0]
        # The start of a plain-HTTP response, for its status line in the log.
        response = bytearray()
        outcome = "ok"
        upstream_writer = None
        try:
            request = await self._read_request(reader)
            if request.host == host.HOST:
                ip = "host"
                outcome = await self._host(request, reader, writer)
                return
            ip, upstream_reader, upstream_writer = await self._open(request)
            if request.upstream_head is None:
                writer.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            else:
                upstream_writer.write(request.upstream_head)
                up[0] += len(request.upstream_head)
            await _relay(reader, writer, upstream_reader, upstream_writer, up, down,
                         response if request.upstream_head is not None else None)
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
            target = f"{request.method} {request.host}:{request.port}{request.path}" if request else "-"
            if request is not None and request.injected:
                target += " +credential"
            status = _status(response)
            log.log(
                logging.INFO if outcome == "ok" else logging.WARNING,
                "%s %s -> %s %s%s up=%d down=%d %.2fs",
                self.name, target, ip or "-", outcome, f" {status}" if status else "",
                up[0], down[0], time.monotonic() - started,
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
        injection = next((i for i in self.injections if i.matches(url.hostname)), None)
        if injection is not None:
            # Whatever the bottle sent under this name is a stand-in; replace it.
            # The Host header goes with the destination too, so a request
            # carrying a credential can't ask one server to answer as another.
            dropped = {injection.header.lower(), "host"}
            headers = [line for line in headers if line.split(":", 1)[0].strip().lower() not in dropped]
            authority = url.hostname + (f":{url.port}" if url.port else "")
            headers[:0] = [f"Host: {authority}"]
            headers.append(f"{injection.header}: {injection.value}")
        if not any(line.lower().startswith("host:") for line in headers):
            headers.insert(0, f"Host: {url.netloc}")
        # One request per connection keeps relaying simple: the upstream closes when done.
        headers.append("Connection: close")
        upstream_head = "\r\n".join([f"{method} {path} {version}", *headers, "", ""]).encode("latin-1")
        parsed = {}
        for line in header_lines:
            name, _, value = line.partition(":")
            parsed[name.strip().lower()] = value.strip()
        default_port = injection.port if injection else 80
        return Request(
            method, url.hostname, url.port or default_port, upstream_head, url.path or "/", url.query, parsed,
            injected=injection is not None,
        )

    async def _open(self, request: Request) -> tuple[str, asyncio.StreamReader, asyncio.StreamWriter]:
        host, port = request.host, request.port
        try:
            addresses = await self.resolve(host, port)
        except OSError:
            raise Refused(502, "Bad Gateway: cannot resolve host", "unresolved") from None
        # Connect to the exact addresses we checked, so a second lookup can't swap them.
        allowed = [a for a in addresses if self.policy.allows(host, ipaddress.ip_address(a), private=request.injected)]
        if not allowed:
            raise Refused(403, "Forbidden: destination not allowed", "denied")
        # The machine's end of an injected request is always TLS: a credential is
        # never put on a request that leaves here in the clear.
        context = _tls_context() if request.injected else None
        for address in allowed:
            try:
                connect = asyncio.open_connection(address, port, ssl=context, server_hostname=host if context else None)
                reader, writer = await asyncio.wait_for(connect, CONNECT_TIMEOUT)
                return address, reader, writer
            except ssl.SSLError as e:
                raise Refused(502, f"Bad Gateway: TLS to {host} failed", "tls-failed") from e
            except (OSError, TimeoutError):
                continue
        raise Refused(502, "Bad Gateway: cannot connect", "failed")


@functools.cache
def _tls_context() -> ssl.SSLContext:
    """Verifying TLS with the machine's trusted roots, for the host end of an injected request."""
    return ssl.create_default_context()


async def _resolve(host: str, port: int) -> list[str]:
    infos = await asyncio.get_running_loop().getaddrinfo(host, port, type=socket.SOCK_STREAM)
    return list(dict.fromkeys(info[4][0] for info in infos))


async def _relay(
    client_r, client_w, upstream_r, upstream_w, up: list[int], down: list[int], response: bytearray | None = None,
) -> None:
    """Copy both ways until the upstream is done.

    The client finishing only half-closes the upstream (it may still be
    answering); the upstream finishing ends the exchange. `response`, if
    given, collects the start of what the upstream sends.
    """
    sending = asyncio.create_task(_pipe(client_r, upstream_w, up))
    try:
        await _pipe(upstream_r, client_w, down, response)
    finally:
        sending.cancel()


STATUS_PEEK = 256


async def _pipe(src: asyncio.StreamReader, dst: asyncio.StreamWriter, count: list[int], start: bytearray | None = None) -> None:
    try:
        while data := await src.read(64 * 1024):
            if start is not None and len(start) < STATUS_PEEK:
                start += data[:STATUS_PEEK - len(start)]
            dst.write(data)
            await dst.drain()
            count[0] += len(data)
        if dst.can_write_eof():
            dst.write_eof()
    except (ConnectionError, OSError):
        pass


def _status(response: bytes) -> str | None:
    """The status code of an HTTP response that starts with `response`, e.g. "401".

    Interim responses (100 Continue) are skipped for the final one, when it's
    within what was collected.
    """
    status, rest = None, bytes(response)
    while rest:
        line, _, _ = rest.partition(b"\r\n")
        parts = line.split(b" ")
        if len(parts) < 2 or not parts[0].startswith(b"HTTP/") or not parts[1].isdigit():
            break
        status = parts[1].decode()
        head_end = rest.find(b"\r\n\r\n")
        if not status.startswith("1") or head_end < 0:
            break
        rest = rest[head_end + 4:]
    return status


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
