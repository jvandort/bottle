"""Egress: a bottle's only way out.

A bottle's driver (smolvm, see smolvm.py) carries the guest's sockets and asks
bottled about each TCP connection before it connects anywhere (see decider.py).
Every connection is a flow (Flow): the address the bottle connected to, and the
first bytes it sent. Gateway.decide says whether it may go there directly
(the driver makes the connection from the host, with the host's routes and VPN), whether
bottled takes it, or neither; Gateway.serve is the far end of one bottled
takes. The policy (Policy) is enforced where root in the bottle can't reach it.

bottled takes two kinds of flow. Those to bottle.host (host.ADDRESS) it
answers itself, as the git service (see host.py). And it holds credentials the
bottle never sees (Injection): for the hosts a feature names, it terminates
the bottle's TLS with a certificate from the egress CA (see ca.py), puts the
real token in place of the stand-in on the requests that present one, and
makes the HTTPS connection to the server itself. For the hosts a feature
caches (see cache.py), it terminates the bottle's TLS the same way, and
answers GETs from what earlier bottles downloaded.
"""

import asyncio
import base64
import binascii
import fnmatch
import functools
import ipaddress
import logging
import os
import socket
import ssl
import time
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass
from typing import BinaryIO
from urllib.parse import urljoin, urlsplit

from bottle import cache, host

log = logging.getLogger("bottle.egress")
# Silent unless the application configures logging (the CLI does).
log.addHandler(logging.NullHandler())

MAX_HEADER_BYTES = 64 * 1024
HEADER_TIMEOUT = 30
CONNECT_TIMEOUT = 10
# Hop-by-hop headers, for this connection only, never forwarded upstream.
HOP_HEADERS = {"proxy-connection", "proxy-authorization", "connection", "keep-alive"}
# Headers a proxy or load balancer sets to tell the server where a request came
# from, what it was really for, or which method it is. A bottle's client has no
# reason to send them, and on a request carrying a credential they could make
# the server (or one in front of it) route or read it as something else, so
# they're dropped there, as Host is replaced.
ROUTING_HEADERS = {
    "forwarded", "x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "x-forwarded-port",
    "x-forwarded-prefix", "x-forwarded-server", "x-forwarded-scheme", "x-forwarded-ssl", "x-real-ip",
    "x-original-url", "x-rewrite-url", "x-original-host", "x-host",
    "x-http-method-override", "x-http-method", "x-method-override",
}
# Methods a server answers by echoing the request back, credential included,
# so they're never sent with one. TRACK is IIS's TRACE.
ECHOING_METHODS = {"TRACE", "TRACK"}
# A cached host is reached over https, on its own port.
CACHE_PORT = 443
# How long a connection to a cached host is kept open for its next request.
KEEP_ALIVE_TIMEOUT = 60
# Redirects bottled follows for a download it keeps, which is kept as if it had never been redirected.
REDIRECTS = {301, 302, 303, 307, 308}
MAX_REDIRECTS = 5
# Headers whose request may be answered for whoever sent it alone, so the answer is never kept or shared.
PERSONAL_HEADERS = {"authorization", "cookie"}

IPAddress = ipaddress.IPv4Address | ipaddress.IPv6Address
Resolver = Callable[[str, int], Awaitable[list[str]]]
# A TLS server context presenting a certificate for a host (ca.server_context).
Certificates = Callable[[str], ssl.SSLContext]


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

    The bottle talks to an injected host the way it talks to any other: HTTPS,
    straight to it. TLS is opaque, and a header can only be attached to a
    request bottled can read, so for this host and port the decider redirects
    the flow to bottled, which completes the TLS handshake itself, with a certificate from the
    bottle's egress CA, which it trusts for its credentials' hosts only (see
    ca.py), reads each request, and sends it on over its own verified TLS
    connection. A credential is never on the wire in the clear, in the bottle
    or beyond it.

    The credential goes only on a request that asks for it: one whose header
    presents the stand-in, the fake token a tool in the bottle was given (see
    features.py) -- as the header's token, or as the password of Basic
    credentials, which is how git sends what a credential helper gave it. Such
    a request gets the real one in the same form: `value` in place of the
    token, or the same Basic user with `credential` as its password. Any other
    request goes on as the bottle sent it, so a client that wasn't asked to
    authenticate -- a build tool downloading a release, an anonymous clone --
    reaches the host as it would from the machine. That is about being
    understood, not about who may spend the credential: the stand-in is no
    secret, and anything in the bottle can present it.

    An injected host is also reachable where the policy would otherwise refuse
    a private address, since naming the host is what configuring the
    credential means. Nothing else about that host opens up: a flow to
    another port goes direct, and is judged by the policy alone.
    """

    host: str  # a pattern, e.g. "teamcity.corp.example.com" or "*.example.com"
    header: str
    value: str  # the whole header value, credential included
    port: int = 443
    standin: str = ""  # what a request presents to be given the credential; "" matches none
    credential: str = ""  # the bare credential, for Basic; "" when the feature doesn't take Basic

    def replacing(self, presented: str) -> str | None:
        """The value to send in place of `presented`, this injection's header as the bottle sent it; None to leave it."""
        if not self.standin:
            return None
        scheme, _, token = presented.strip().partition(" ")
        if self.standin in (scheme, token.strip()):
            return self.value
        if scheme.lower() == "basic" and self.credential and self.header.lower() == "authorization":
            try:
                user, _, password = base64.b64decode(token.strip(), validate=True).decode().partition(":")
            except (binascii.Error, UnicodeDecodeError):
                return None
            if password == self.standin:
                return "Basic " + base64.b64encode(f"{user}:{self.credential}".encode()).decode()
        return None

    def matches(self, host: str, port: int) -> bool:
        return port == self.port and fnmatch.fnmatch(host.lower(), self.host.lower())


class Refused(Exception):
    """The request can't be served; carries the HTTP status to answer with, None when there's no answering."""

    def __init__(self, status: int | None, reason: str, outcome: str) -> None:
        super().__init__(reason)
        self.status = status
        self.reason = reason
        self.outcome = outcome


@dataclass
class Request:
    method: str
    host: str
    port: int
    # For plain HTTP: the request head to send upstream. None for the TLS around a credential's requests.
    upstream_head: bytes | None
    # For plain HTTP: the path, query and headers (lowercase names), for host services.
    path: str = ""
    query: str = ""
    headers: dict[str, str] | None = None
    # Set for a request inside TLS bottled terminated, for a host a
    # credential names: bottled makes this connection over TLS, and allows a
    # private address for this destination.
    terminated: bool = False
    # Set when the credential was attached, which only a terminated request can have.
    injected: bool = False


@dataclass(frozen=True)
class Flow:
    """One connection a bottle opened, as the driver presents it before connecting anywhere.

    The address the guest connected to, and the first bytes it sent, where a name
    may be: a TLS ClientHello's SNI, an HTTP request's Host.
    """

    address: str
    port: int
    first: bytes = b""  # the first bytes it sent, already read


# What a redirected flow is for.
GIT, CREDENTIAL, CACHE = "git", "credential", "cache"
# bottled reads the rest of a TLS flow's opening itself, then decides it: the
# driver showed too little of it to tell whether a credential's host is its name.
OPENING = "opening"


@dataclass(frozen=True)
class Direct:
    """The flow may go straight to its address, its bytes untouched: the driver connects it."""

    addresses: tuple[str, ...]


@dataclass(frozen=True)
class Redirect:
    """bottled is the flow's far end: as bottle.host's git service, to attach `injection`'s credential, to
    answer from `cached`'s store, or to read the rest of a ClientHello the driver didn't wait for
    (OPENING), and decide then.

    `addresses` are where the credential's or cached host may be reached, already judged.
    """

    service: str
    injection: Injection | None = None
    addresses: tuple[str, ...] = ()
    cached: cache.Cached | None = None


class Gateway:
    """One bottle's way out: what decides its flows, and serves those it keeps."""

    def __init__(
        self,
        name: str,
        policy: Policy,
        resolve: Resolver | None = None,
        services: "host.HostServices | None" = None,
        injections: tuple[Injection, ...] = (),
        certificates: Certificates | None = None,
        caches: tuple[cache.Cached, ...] = (),
        store: cache.Store | None = None,
    ) -> None:
        self.name = name
        self.policy = policy
        self.resolve = resolve or _resolve
        # bottle.host's git service: flows to host.ADDRESS are answered here, never forwarded.
        self.services = services
        # Credentials the host attaches for the bottle, by destination, and
        # the certificates that let it: without them, nothing is attached.
        self.injections = tuple(injections) if certificates else ()
        self.certificates = certificates
        # Hosts whose downloads are kept, in `store`: also only with certificates to read them by.
        self.caches = tuple(caches) if certificates and store else ()
        self.store = store

    def log_flow(self, target: str, ip: str | None, outcome: str, status: str | None, up: list[int], down: list[int],
             started: float) -> None:
        log.log(
            logging.INFO if outcome == "ok" else logging.WARNING,
            "%s %s -> %s %s%s up=%d down=%d %.2fs",
            self.name, target, ip or "-", outcome, f" {status}" if status else "",
            up[0], down[0], time.monotonic() - started,
        )

    async def decide(self, flow: "Flow", opening_read: bool = False) -> "Direct | Redirect":
        """What to do with `flow`: where it may go directly, or which of bottled's services takes it.

        Refused if it may go nowhere. Judged by the address the bottle connected to,
        and by a name read from its first bytes, which is the bottle's claim,
        so it's checked before it opens anything. `opening_read`: bottled has
        already read all of the flow's opening it's going to.
        """
        name = name_in(flow.first)
        address = ipaddress.ip_address(flow.address)
        if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped:
            address = address.ipv4_mapped  # judged as the IPv4 address it is
        if address == ipaddress.ip_address(host.ADDRESS):
            if flow.port == 80 and self.services is not None:
                return Redirect(GIT)
            raise Refused(403, "Forbidden: no such host service", "denied")
        terminated_here = any(i.port == flow.port for i in self.injections) or (self.caches and flow.port == CACHE_PORT)
        if name is None and not opening_read and _opening_unread(flow.first) and terminated_here:
            # smolvm asks with what arrived in its first 100ms, and a busy guest's
            # ClientHello can come later: let through unread, a credential's host
            # would get the bottle's stand-in, never the credential, and a cached
            # host's downloads wouldn't be kept.
            if not self.policy.allows("", address, private=True):
                raise Refused(403, "Forbidden: destination not allowed", "denied")  # the host itself: no name helps
            return Redirect(OPENING)
        # A credential goes only on TLS, so only a ClientHello's name claims one.
        injection = self._injection_at(name, flow.port) if name and flow.first[:1] == b"\x16" else None
        if injection is not None:
            # Sent to the name, never to the address the bottle chose: the
            # credential goes only where its host's name leads.
            return Redirect(CREDENTIAL, injection, tuple(await self._allowed(name, flow.port, private=True)))
        cached = self._cached_at(name, flow.port) if name and flow.first[:1] == b"\x16" else None
        if cached is not None:
            # Sent to the name too: what's kept under a host's name must be what that host serves.
            return Redirect(CACHE, addresses=tuple(await self._allowed(name, flow.port, private=False)), cached=cached)
        if not self.policy.allows(name or "", address):
            raise Refused(403, "Forbidden: destination not allowed", "denied")
        if not address.is_global and (not name or address not in await self._addresses(name, flow.port)):
            # A private address allowed only for its name: the name must lead there.
            raise Refused(403, "Forbidden: destination not allowed", "denied")
        return Direct((flow.address,))

    async def serve(self, flow: "Flow", decision: "Redirect", reader, writer) -> None:
        """Be the far end of a redirected flow, whose first bytes have already been read from `reader`."""
        started = time.monotonic()
        name = name_in(flow.first)
        target = f"{flow.address}:{flow.port}" + (f" ({name})" if name else "")
        ip, up, down, response = None, [0], [0], bytearray()
        outcome, status = "ok", None
        tls, upstream_writer = None, None
        try:
            if decision.service == OPENING:
                flow = Flow(flow.address, flow.port, await _read_opening(flow.first, reader))
                name = name_in(flow.first)
                target = f"{flow.address}:{flow.port}" + (f" ({name})" if name else "") + " late"
                decision = await self.decide(flow, opening_read=True)
                if isinstance(decision, Direct):
                    # Connected from here, as the driver would have: the bottle's bytes untouched.
                    ip = flow.address
                    upstream_reader, upstream_writer = await self._connect_to(flow.address, flow.port)
                    upstream_writer.write(flow.first)
                    up[0] += len(flow.first)
                    await _relay(reader, writer, upstream_reader, upstream_writer, up, down)
                    return
            if decision.service == GIT:
                ip = "host"
                outcome = await self._serve_git(flow.first, reader, writer)
                return
            context = await self._certificate(name)
            reader, tls = await self._accept_tls(context, flow.first, reader, writer)
            writer = tls
            if decision.service == CACHE:
                target += " cached"
                ip, status = await self._serve_cached(name, decision, reader, writer, up, down)
                return
            request = await self._inside(Request("CONNECT", name, flow.port, None), decision.injection, reader)
            if request.injected:
                target += " +credential"
            ip, upstream_reader, upstream_writer = await self._open(request, list(decision.addresses))
            upstream_writer.write(request.upstream_head)
            up[0] += len(request.upstream_head)
            await _relay(reader, writer, upstream_reader, upstream_writer, up, down, response)
            status = _status(response)
        except Refused as e:
            outcome = e.outcome
            # A bottle speaking TLS can only be answered inside it.
            if e.status is not None and (tls is not None or decision.service == GIT):
                await _respond(writer, e.status, e.reason)
        except Exception:
            outcome = "error"
            log.exception("%s: unexpected error", self.name)
        finally:
            for w in (upstream_writer, writer):
                if w is not None:
                    w.close()
            self.log_flow(target, ip, outcome, status, up, down, started)

    async def _serve_git(self, first: bytes, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> str:
        """Answer an origin-form request to bottle.host, its head in `first` (perhaps not all of it)."""
        source = _Reader(reader.read, first)
        method, target, _, header_lines = await _read_head(source)
        url = urlsplit(target)
        if not target.startswith("/"):
            raise Refused(400, "Bad Request", "bad-request")
        headers = _parsed(header_lines)
        try:
            body = await host.read_body(source, headers)
        except (ValueError, asyncio.IncompleteReadError):
            raise Refused(400, "Bad Request", "bad-request") from None
        return await self.services.handle(host.HttpRequest(method, url.path or "/", url.query, headers, body), writer)

    def _injection_at(self, host: str, port: int) -> Injection | None:
        """The credential for this host and port, if one claims it."""
        return next((i for i in self.injections if i.matches(host, port)), None)

    def _cached_at(self, host: str, port: int) -> cache.Cached | None:
        """How this host's downloads are kept, if they are."""
        return next((c for c in self.caches if c.matches(host)), None) if port == CACHE_PORT else None

    async def _serve_cached(
        self, name: str, decision: Redirect, reader: "_Reader", writer: "_Tls", up: list[int], down: list[int],
    ) -> tuple[str | None, str | None]:
        """Answer the requests a bottle sends a cached host over one connection, from the store where it can.

        GETs and HEADs are answered one after another on the same connection,
        each over a connection of bottled's own to the host when it isn't
        answered from here. Anything else, or anything with a body, is the
        connection's last request, relayed as it was sent. Returns the
        address last connected to and the last status, for the flow's log.
        """
        ip, status, keep = None, None, True
        while keep:
            if status is not None:
                try:
                    if await asyncio.wait_for(reader.at_eof(), KEEP_ALIVE_TIMEOUT):
                        break
                except TimeoutError:
                    break
            method, target, version, header_lines = await _read_head(reader)
            url = urlsplit(target)
            if method == "CONNECT" or (url.scheme and url.scheme != "https") or (url.netloc and url.hostname != name.lower()):
                raise Refused(400, "Bad Request", "bad-request")
            headers = _parsed(header_lines)
            path = url.path or "/"
            path_and_query = path + (f"?{url.query}" if url.query else "")
            keep = version == "HTTP/1.1" and "close" not in _tokens(headers.get("connection", ""))
            if method not in ("GET", "HEAD") or "transfer-encoding" in headers or headers.get("content-length", "0") != "0":
                ip, upstream_reader, upstream_writer = await self._open(
                    Request(method, name, CACHE_PORT, None, terminated=True), list(decision.addresses))
                head = _upstream_head(method, path_and_query, "HTTP/1.1", _to_server(name, header_lines))
                upstream_writer.write(head)
                up[0] += len(head)
                response = bytearray()
                try:
                    await _relay(reader, writer, upstream_reader, upstream_writer, up, down, response)
                finally:
                    upstream_writer.close()
                status = _status(response)
                self._log_request(method, name, path_and_query, "passed", status)
                break
            # Kept only where the answer can't depend on who asked, or how.
            cacheable = (
                not url.query and "range" not in headers and not PERSONAL_HEADERS & headers.keys()
                and not decision.cached.excludes(path) and self.store.file(name, path) is not None
            )
            if cacheable and not _tokens(headers.get("cache-control", "") + "," + headers.get("pragma", "")) & {"no-cache"}:
                hit = await asyncio.to_thread(self.store.open, name, path)
                if hit is not None:
                    with hit:
                        size = os.fstat(hit.fileno()).st_size
                        # No Content-Type: none was kept, and the bottle can tell what a file is as well as bottled can.
                        if method == "HEAD":
                            await _send(writer, 200, "OK", [f"Content-Length: {size}"], None, None, keep, down)
                        else:
                            await _send(writer, 200, "OK", [], _chunks_of(hit), size, keep, down)
                    status = "200"
                    self._log_request(method, name, path, "hit", status)
                    continue
            if cacheable and method == "GET":
                ip, upstream_reader, upstream_writer, response = await self._fetch(
                    name, path, headers.get("user-agent", ""), list(decision.addresses), up)
            else:
                ip, upstream_reader, upstream_writer = await self._open(
                    Request(method, name, CACHE_PORT, None, terminated=True), list(decision.addresses))
                head = _upstream_head(method, path_and_query, "HTTP/1.1", _to_server(name, header_lines))
                upstream_writer.write(head)
                up[0] += len(head)
                response = None
            try:
                response = response or await _read_response(upstream_reader)
                length = _body_length(method, response)
                partial = None
                if cacheable and method == "GET" and _storable(response, length):
                    partial = await asyncio.to_thread(self.store.partial, name, path)
                kept = False
                # No cookies from a cached host: one the bottle sent back would keep its requests from
                # being answered from the store, and those set for bottled's own request, perhaps by a
                # host it was redirected to, were never the bottle's.
                header_lines = [line for line in _to_client(response.header_lines, length == 0)
                                if line.split(":", 1)[0].strip().lower() not in ("set-cookie", "set-cookie2")]
                try:
                    if length == 0:
                        await _send(writer, response.status, response.reason, header_lines, None, None, keep, down)
                    else:
                        body = _body(upstream_reader, length)
                        keep = await _send(
                            writer, response.status, response.reason, header_lines,
                            _teed(body, partial) if partial else body, length if isinstance(length, int) else None,
                            keep and length is not None, down,
                        )
                    kept = partial is not None and await asyncio.to_thread(partial.commit)
                finally:
                    if partial is not None and not kept:
                        await asyncio.to_thread(partial.discard)
            finally:
                upstream_writer.close()
            status = str(response.status)
            self._log_request(method, name, path_and_query, "stored" if kept else "fetched" if cacheable and method == "GET" else "passed", status)
        return ip, status

    async def _fetch(
        self, name: str, path: str, user_agent: str, addresses: list[str], up: list[int],
    ) -> tuple[str, asyncio.StreamReader, asyncio.StreamWriter, "_Response"]:
        """GET `path` from `name` to keep, following redirects: the response, its body still to read.

        The request is bottled's own, not the bottle's, so the file kept for a
        path is the same whoever asked first. A redirect that can't be
        followed (to plain http, or somewhere the policy refuses) is the
        response.
        """
        host, port, target = name, CACHE_PORT, path
        for hop in range(MAX_REDIRECTS + 1):
            ip, upstream_reader, upstream_writer = await self._open(Request("GET", host, port, None, terminated=True), addresses)
            authority = host + (f":{port}" if port != CACHE_PORT else "")
            head = "\r\n".join([
                f"GET {target} HTTP/1.1", f"Host: {authority}", f"User-Agent: {user_agent or 'bottle'}", "Accept: */*",
                "Accept-Encoding: identity", "Connection: close", "", "",
            ]).encode("latin-1")
            upstream_writer.write(head)
            up[0] += len(head)
            try:
                response = await _read_response(upstream_reader)
            except BaseException:
                upstream_writer.close()
                raise
            if response.status not in REDIRECTS or hop == MAX_REDIRECTS:
                return ip, upstream_reader, upstream_writer, response
            try:
                location = urlsplit(urljoin(f"https://{authority}{target}", response.headers.get("location", "")))
                next_port = location.port or CACHE_PORT
                if location.scheme != "https" or not location.hostname:
                    return ip, upstream_reader, upstream_writer, response
                addresses = await self._allowed(location.hostname, next_port, private=False)
            except (Refused, ValueError):  # a Location that isn't a URL has a port that isn't one
                return ip, upstream_reader, upstream_writer, response
            upstream_writer.close()
            host, port = location.hostname, next_port
            target = (location.path or "/") + (f"?{location.query}" if location.query else "")
        raise AssertionError("the last hop is always returned")

    def _log_request(self, method: str, name: str, path: str, outcome: str, status: str | None) -> None:
        log.info("%s %s https://%s%s %s %s", self.name, method, name, path, outcome, status or "-")

    async def _certificate(self, name: str) -> ssl.SSLContext:
        """A TLS server context presenting the egress CA's certificate for `name`."""
        try:
            return await asyncio.to_thread(self.certificates, name)
        except Exception:
            log.exception("%s: no certificate for %s", self.name, name)
            raise Refused(502, f"Bad Gateway: no certificate for {name}", "error") from None

    async def _accept_tls(
        self, context: ssl.SSLContext, first: bytes, reader: asyncio.StreamReader, writer: asyncio.StreamWriter,
    ) -> tuple["_Reader", "_Tls"]:
        """Be the TLS server for a bottle whose handshake starts with `first`, already read from `reader`."""
        tls = _Tls(context, first, reader, writer)
        try:
            await asyncio.wait_for(tls.handshake(), HEADER_TIMEOUT)
        except (ssl.SSLError, ConnectionError, OSError, TimeoutError):
            # Most likely a client that doesn't trust the egress CA: nothing to answer on.
            writer.close()
            raise Refused(None, "TLS handshake with the bottle failed", "tls-refused") from None
        return _Reader(tls.read), tls

    async def _inside(self, tunnel: Request, injection: Injection, reader: "_Reader") -> Request:
        """The request a bottle sent inside TLS terminated for `tunnel`'s host, made ready for the upstream."""
        method, target, version, header_lines = await _read_head(reader)
        url = urlsplit(target)
        if method == "CONNECT" or (url.scheme and url.scheme != "https") or (url.netloc and url.hostname != tunnel.host.lower()):
            raise Refused(400, "Bad Request", "bad-request")
        if method.upper() in ECHOING_METHODS:
            # The response would carry the credential back into the bottle.
            raise Refused(405, "Method Not Allowed", "denied")
        path = (url.path or "/") + (f"?{url.query}" if url.query else "")
        # The credential, if the bottle presented the stand-in under this name.
        presented = [line.split(":", 1)[1] for line in header_lines
                     if line.split(":", 1)[0].strip().lower() == injection.header.lower()]
        value = next((v for v in map(injection.replacing, presented) if v is not None), None)
        # The Host header goes with the destination, and the routing headers
        # go altogether, so a request carrying a credential can't ask one
        # server to answer as another, or for something else. Nothing else
        # differs between the requests that carry it and those that don't.
        dropped = {"host", *ROUTING_HEADERS} | ({injection.header.lower()} if value is not None else set())
        headers = [line for line in _forwarded(header_lines) if line.split(":", 1)[0].strip().lower() not in dropped]
        authority = tunnel.host + (f":{tunnel.port}" if tunnel.port != 443 else "")
        headers = [f"Host: {authority}", *headers]
        if value is not None:
            headers.append(f"{injection.header}: {value}")
        return Request(
            method, tunnel.host, tunnel.port, _upstream_head(method, path, version, headers),
            url.path or "/", url.query, _parsed(header_lines), terminated=True, injected=value is not None,
        )

    async def _connect_to(self, address: str, port: int) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        """A plain connection to an address already judged."""
        try:
            return await asyncio.wait_for(asyncio.open_connection(address, port), CONNECT_TIMEOUT)
        except (OSError, TimeoutError):
            raise Refused(None, "cannot connect", "failed") from None

    async def _addresses(self, name: str, port: int) -> set[IPAddress]:
        """Where `name` leads that the policy allows, as addresses, so IPv6's many spellings compare equal."""
        return {ipaddress.ip_address(a.partition("%")[0]) for a in await self._allowed(name, port, private=False)}

    async def _allowed(self, host: str, port: int, private: bool) -> list[str]:
        """The addresses `host` resolves to that the policy lets the bottle reach; refused if none."""
        try:
            addresses = await self.resolve(host, port)
        except OSError:
            raise Refused(502, "Bad Gateway: cannot resolve host", "unresolved") from None
        allowed = [a for a in addresses if self.policy.allows(host, ipaddress.ip_address(a), private=private)]
        if not allowed:
            raise Refused(403, "Forbidden: destination not allowed", "denied")
        return allowed

    async def _open(
        self, request: Request, allowed: list[str] | None = None,
    ) -> tuple[str, asyncio.StreamReader, asyncio.StreamWriter]:
        """Connect to the request's destination, at `allowed` when it has already been judged."""
        host, port = request.host, request.port
        # Connect to the exact addresses we checked, so a second lookup can't swap them.
        if allowed is None:
            allowed = await self._allowed(host, port, private=request.terminated)
        # The machine's end of a terminated tunnel is always TLS: a credential
        # is never put on a request that leaves here in the clear, and one
        # without it was TLS when the bottle sent it.
        context = _tls_context() if request.terminated else None
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


async def _read_head(reader: asyncio.StreamReader) -> tuple[str, str, str, list[str]]:
    """A request's method, target, HTTP version and header lines."""
    try:
        head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), HEADER_TIMEOUT)
    except asyncio.LimitOverrunError:
        raise Refused(431, "Request Header Fields Too Large", "bad-request") from None
    except (asyncio.IncompleteReadError, TimeoutError, ConnectionError, ssl.SSLError):
        raise Refused(400, "Bad Request", "bad-request") from None
    request_line, *header_lines = head.decode("latin-1").split("\r\n")[:-2]
    parts = request_line.split(" ")
    if len(parts) != 3 or not parts[2].startswith("HTTP/"):
        raise Refused(400, "Bad Request", "bad-request")
    method, target, version = parts
    return method, target, version, header_lines


def _forwarded(header_lines: list[str]) -> list[str]:
    """The header lines meant for the server, without the hop-by-hop ones."""
    return [line for line in header_lines if line.split(":", 1)[0].strip().lower() not in HOP_HEADERS]


def _parsed(header_lines: list[str]) -> dict[str, str]:
    """Headers by lowercase name; one sent on several lines is one, its values joined with commas, as HTTP has it."""
    parsed: dict[str, str] = {}
    for line in header_lines:
        name, _, value = line.partition(":")
        name, value = name.strip().lower(), value.strip()
        parsed[name] = f"{parsed[name]}, {value}" if name in parsed else value
    return parsed


def _tokens(value: str) -> set[str]:
    """A comma-separated header value's tokens, lowercase: Connection's, or Cache-Control's."""
    return {token.strip().lower() for token in value.split(",") if token.strip()}


def _to_server(name: str, header_lines: list[str]) -> list[str]:
    """A bottle's header lines for a request to `name`, which it gets as its Host, without the routing ones."""
    dropped = {"host", *ROUTING_HEADERS}
    return [f"Host: {name}", *(line for line in _forwarded(header_lines) if line.split(":", 1)[0].strip().lower() not in dropped)]


def _to_client(header_lines: list[str], bodiless: bool) -> list[str]:
    """A server's header lines for the bottle, without the framing bottled sets itself.

    A response with no body (to HEAD, a 304) keeps its Content-Length, which
    is the length of the body it's about, not of its own.
    """
    dropped = HOP_HEADERS | {"transfer-encoding"} | (set() if bodiless else {"content-length"})
    return [line for line in header_lines if line.split(":", 1)[0].strip().lower() not in dropped]


def _upstream_head(method: str, path: str, version: str, headers: list[str]) -> bytes:
    # One request per connection keeps relaying simple: the upstream closes when done.
    return "\r\n".join([f"{method} {path} {version}", *headers, "Connection: close", "", ""]).encode("latin-1")


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


# --- responses read and sent ------------------------------------------------------


@dataclass(frozen=True)
class _Response:
    """A server's response head: its status, reason and header lines, and the headers parsed (lowercase names)."""

    status: int
    reason: str
    header_lines: list[str]
    headers: dict[str, str]


# A body framed by chunked transfer coding, not by its length.
CHUNKED = "chunked"


async def _read_response(reader: asyncio.StreamReader) -> _Response:
    """The head of a server's final response: interim ones (100 Continue) are skipped."""
    while True:
        try:
            head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), HEADER_TIMEOUT)
        except (asyncio.LimitOverrunError, asyncio.IncompleteReadError, TimeoutError, ConnectionError, ssl.SSLError):
            raise Refused(502, "Bad Gateway: no response from the server", "failed") from None
        status_line, *header_lines = head.decode("latin-1").split("\r\n")[:-2]
        parts = status_line.split(" ", 2)
        if len(parts) < 2 or not parts[0].startswith("HTTP/1.") or not parts[1].isdigit() or len(parts[1]) != 3:
            raise Refused(502, "Bad Gateway: the server's response isn't HTTP", "failed")
        status = int(parts[1])
        if 100 <= status < 200 and status != 101:
            continue
        return _Response(status, parts[2] if len(parts) > 2 else "", header_lines, _parsed(header_lines))


def _body_length(method: str, response: _Response) -> int | str | None:
    """How the response's body ends: its length (0 for none), CHUNKED, or None, when the server closes."""
    if method == "HEAD" or response.status in (101, 204, 304):
        return 0
    if "chunked" in _tokens(response.headers.get("transfer-encoding", "")):
        return CHUNKED
    length = response.headers.get("content-length")
    if length is None:
        return None
    if not length.isdigit():
        raise Refused(502, "Bad Gateway: the server's response has no length bottled can read", "failed")
    return int(length)


def _storable(response: _Response, length: int | str | None) -> bool:
    """Whether the response to a GET can be kept: a 200 of a file as it is, which is known to be all of it.

    Not when the server says not to keep it, or to check with it before using
    it again (no-cache, max-age=0): a kept file is never checked again.
    """
    directives = _tokens(response.headers.get("cache-control", "")).union(
        {"no-cache"} if "no-cache" in _tokens(response.headers.get("pragma", "")) else set())
    return (
        response.status == 200 and length is not None
        and response.headers.get("content-encoding", "identity").lower() == "identity"
        and not directives & {"no-store", "no-cache", "private", "max-age=0", "s-maxage=0"}
    )


async def _body(reader: asyncio.StreamReader, length: int | str | None) -> AsyncIterator[bytes]:
    """A response's body, as it arrives, without its framing; refused if it ends early."""
    try:
        if length == CHUNKED:
            while True:
                size = int((await reader.readuntil(b"\n")).split(b";")[0].strip(), 16)
                if size == 0:
                    while (await reader.readuntil(b"\n")).strip():  # trailers, until the empty line
                        pass
                    return
                async for data in _exactly(reader, size):
                    yield data
                if (await reader.readuntil(b"\n")).strip():
                    raise ValueError("a chunk longer than it said")
        elif length is None:
            while data := await reader.read(64 * 1024):
                yield data
        else:
            async for data in _exactly(reader, length):
                yield data
    except (ValueError, asyncio.IncompleteReadError, asyncio.LimitOverrunError, ConnectionError, ssl.SSLError):
        raise Refused(None, "the server's response ended early", "failed") from None


async def _exactly(reader: asyncio.StreamReader, n: int) -> AsyncIterator[bytes]:
    while n:
        data = await reader.read(min(n, 64 * 1024))
        if not data:
            raise asyncio.IncompleteReadError(b"", n)
        n -= len(data)
        yield data


async def _chunks_of(file: BinaryIO) -> AsyncIterator[bytes]:
    while data := file.read(64 * 1024):
        yield data


async def _teed(body: AsyncIterator[bytes], partial: cache.Partial) -> AsyncIterator[bytes]:
    """`body`, written to `partial` too."""
    async for data in body:
        partial.write(data)
        yield data


async def _send(
    writer, status: int, reason: str, header_lines: list[str], body: AsyncIterator[bytes] | None,
    length: int | None, keep: bool, down: list[int],
) -> bool:
    """Send the bottle a response, framed here: whether the connection can take another request.

    A body of unknown length is chunked when the connection is kept, and
    ends with it when it isn't. Refused, with nothing to answer, if the
    bottle goes away.
    """
    lines = [f"HTTP/1.1 {status} {reason}".rstrip(), *header_lines]
    chunked = body is not None and length is None and keep
    if body is not None and length is not None:
        lines.append(f"Content-Length: {length}")
    elif chunked:
        lines.append("Transfer-Encoding: chunked")
    keep = keep and (body is None or length is not None or chunked)
    lines.append("Connection: keep-alive" if keep else "Connection: close")
    try:
        writer.write("\r\n".join([*lines, "", ""]).encode("latin-1"))
        if body is not None:
            async for data in body:
                writer.write(b"%x\r\n%s\r\n" % (len(data), data) if chunked else data)
                await writer.drain()
                down[0] += len(data)
            if chunked:
                writer.write(b"0\r\n\r\n")
        await writer.drain()
    except (ConnectionError, OSError):
        raise Refused(None, "the bottle went away", "closed") from None
    return keep


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


# --- names in a flow's first bytes ---------------------------------------------


def _opening_unread(first: bytes) -> bool:
    """Whether `first` may be the start of a ClientHello not all here yet: nothing, or part of a TLS record."""
    return not first or (first[:1] == b"\x16" and not _record_complete(first))


def _record_complete(data: bytes) -> bool:
    return len(data) >= 5 and len(data) >= 5 + int.from_bytes(data[3:5], "big")


async def _read_opening(first: bytes, reader: asyncio.StreamReader) -> bytes:
    """`first`, and what follows it until it holds a whole TLS record, or isn't TLS, or HEADER_TIMEOUT passes."""
    data = bytearray(first)

    async def more() -> None:
        while (not data or data[:1] == b"\x16") and not _record_complete(data) and len(data) < MAX_HEADER_BYTES:
            chunk = await reader.read(64 * 1024)
            if not chunk:
                return
            data.extend(chunk)

    try:
        await asyncio.wait_for(more(), HEADER_TIMEOUT)
    except TimeoutError:
        pass
    return bytes(data)


def name_in(first: bytes) -> str | None:
    """The destination's name in a flow's first bytes: a TLS ClientHello's SNI, or an HTTP request's Host."""
    return sni(first) if first[:1] == b"\x16" else http_host(first)


def sni(record: bytes) -> str | None:
    """The server name in a TLS ClientHello record, or None if there's none, or it isn't one.

    What the bottle sent, so anything may be here: every length is checked
    against what's there before it's followed.
    """
    try:
        if record[0] != 0x16 or len(record) < 9 or record[5] != 0x01:  # handshake record, ClientHello
            return None
        body = record[9:5 + int.from_bytes(record[3:5], "big")]
        at = 2 + 32  # client version, random
        at += 1 + body[at]  # session id
        at += 2 + int.from_bytes(body[at:at + 2], "big")  # cipher suites
        at += 1 + body[at]  # compression methods
        end = min(len(body), at + 2 + int.from_bytes(body[at:at + 2], "big"))
        at += 2
        while at + 4 <= end:
            kind, length = int.from_bytes(body[at:at + 2], "big"), int.from_bytes(body[at + 2:at + 4], "big")
            at += 4
            if kind == 0:  # server_name: a list of (type, name)
                names, inner = body[at:at + length], 2
                while inner + 3 <= len(names):
                    name_type, name_length = names[inner], int.from_bytes(names[inner + 1:inner + 3], "big")
                    name = names[inner + 3:inner + 3 + name_length]
                    if name_type == 0 and len(name) == name_length:
                        return _hostname(name)
                    inner += 3 + name_length
                return None
            at += length
    except IndexError:
        pass
    return None


def http_host(head: bytes) -> str | None:
    """The Host of an HTTP request whose head starts `head`, without its port; None if it has none."""
    lines = head.split(b"\r\n")
    request_line = lines[0].split(b" ")
    if len(request_line) != 3 or not request_line[2].startswith(b"HTTP/"):
        return None
    for line in lines[1:-1]:  # the last may be cut off
        if not line:
            break
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"host":
            value = value.strip()
            if value.startswith(b"["):
                value = value[1:value.find(b"]")] if b"]" in value else b""
            else:
                value = value.rpartition(b":")[0] if b":" in value else value
            return _hostname(value)
    return None


def _hostname(name: bytes) -> str | None:
    try:
        text = name.decode("ascii").rstrip(".").lower()
    except UnicodeDecodeError:
        return None
    return text if text and len(text) <= 253 and all(c.isalnum() or c in "-.:_" for c in text) else None


# --- streams --------------------------------------------------------------------


class _Reader:
    """The parts of asyncio.StreamReader this module uses, over chunks from `receive`, starting with `first`.

    For a stream whose first bytes have already been read (a decider's flow),
    and for the plaintext inside TLS this module terminates itself. Reads
    from `receive` only when asked, so a slow consumer slows the sender.
    """

    def __init__(self, receive: Callable[[int], Awaitable[bytes]], first: bytes = b"", limit: int = MAX_HEADER_BYTES):
        self._receive = receive
        self._buffer = bytearray(first)
        self._eof = False
        self._limit = limit

    async def _more(self) -> bool:
        if self._eof:
            return False
        data = await self._receive(64 * 1024)
        if not data:
            self._eof = True
            return False
        self._buffer += data
        return True

    async def read(self, n: int = -1) -> bytes:
        if n < 0:
            while await self._more():
                pass
            n = len(self._buffer)
        elif not self._buffer:
            await self._more()
        data = bytes(self._buffer[:n])
        del self._buffer[:n]
        return data

    async def at_eof(self) -> bool:
        """Whether there's nothing more to read: waits for more, or for the end."""
        return not self._buffer and not await self._more()

    async def readuntil(self, separator: bytes = b"\n") -> bytes:
        while (at := self._buffer.find(separator)) < 0:
            if len(self._buffer) > self._limit:
                raise asyncio.LimitOverrunError("separator not found within the limit", len(self._buffer))
            if not await self._more():
                partial = bytes(self._buffer)
                self._buffer.clear()
                raise asyncio.IncompleteReadError(partial, None)
        return await self.read(at + len(separator))

    async def readline(self) -> bytes:
        try:
            return await self.readuntil(b"\n")
        except asyncio.IncompleteReadError as e:
            return e.partial

    async def readexactly(self, n: int) -> bytes:
        while len(self._buffer) < n:
            if not await self._more():
                partial = bytes(self._buffer)
                self._buffer.clear()
                raise asyncio.IncompleteReadError(partial, n)
        return await self.read(n)


class _Tls:
    """The server end of a bottle's TLS, over a connection whose first bytes (its ClientHello) were already read.

    asyncio's start_tls can't be given bytes already taken off the
    connection, so this runs an SSLObject over memory BIOs instead. Its
    plaintext is read through `read` (a _Reader's source) and written like a
    StreamWriter's.
    """

    def __init__(self, context: ssl.SSLContext, first: bytes, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self._incoming, self._outgoing = ssl.MemoryBIO(), ssl.MemoryBIO()
        self._ssl = context.wrap_bio(self._incoming, self._outgoing, server_side=True)
        self._reader, self._writer = reader, writer
        if first:
            self._incoming.write(first)

    def _send(self) -> None:
        if data := self._outgoing.read():
            self._writer.write(data)

    async def _receive(self) -> None:
        await self._writer.drain()
        data = await self._reader.read(64 * 1024)
        if data:
            self._incoming.write(data)
        else:
            self._incoming.write_eof()

    async def handshake(self) -> None:
        while True:
            try:
                self._ssl.do_handshake()
                break
            except ssl.SSLWantReadError:
                self._send()
                await self._receive()
        self._send()
        await self._writer.drain()

    async def read(self, n: int) -> bytes:
        while True:
            try:
                data = self._ssl.read(n)
                self._send()
                return data
            except ssl.SSLWantReadError:
                self._send()
                await self._receive()
            except (ssl.SSLZeroReturnError, ssl.SSLEOFError):
                return b""

    def write(self, data: bytes) -> None:
        self._ssl.write(data)
        self._send()

    async def drain(self) -> None:
        await self._writer.drain()

    def can_write_eof(self) -> bool:
        return False  # TLS has no half-close

    def write_eof(self) -> None:
        pass

    def close(self) -> None:
        try:
            self._ssl.unwrap()  # close_notify
        except (ssl.SSLError, ValueError):
            pass
        self._send()
        self._writer.close()
