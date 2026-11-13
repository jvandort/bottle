"""Egress: the connections a bottle's machine routes to folio serve, and what it does with them.

bottle-cli carries the guest's sockets and decides each TCP connection itself,
by its machine's policy, which routes three kinds to folio serve (see
services.py), each a flow (Flow): the address the bottle connected to, the
name it asked for, and whether its TLS was taken off. A bottle's Gateway is
the far end of its flows.

Those to bottle.host (host.ADDRESS) it answers itself, as the git service
(see host.py); those to port 22 of the rest of host.SSH_NETWORK it joins to
sshd in the bottle the address was granted for, if any. And it holds
credentials the bottle never sees (Injection): for the hosts a feature names,
bottle-cli takes the bottle's TLS off with its machine's CA, and the gateway
puts the real token in place of the stand-in on the requests that present
one, and makes the HTTPS connection to the server itself. Some of a
credential's requests are refused outright (Route): the ones that would turn
the credential into another one the bottle could hold.
"""

import asyncio
import base64
import binascii
import fnmatch
import functools
import ipaddress
import json
import logging
import re
import socket
import ssl
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from urllib.parse import unquote, urlsplit

from bottle import host

log = logging.getLogger("bottle.egress")
# Silent unless the application configures logging (the CLI does).
log.addHandler(logging.NullHandler())

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
# The most of a request's body folio serve reads to put the credential in it (Rewrite.json).
MAX_BODY_BYTES = 1024 * 1024

IPAddress = ipaddress.IPv4Address | ipaddress.IPv6Address
Resolver = Callable[[str, int], Awaitable[list[str]]]


def reachable(ip: IPAddress) -> bool:
    """Whether a credential's host may be reached at `ip`: anywhere but this machine itself and its link (loopback,
    link-local, multicast, unspecified). A private address is expected: naming the host is what configuring the
    credential means."""
    if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped:
        ip = ip.ipv4_mapped
    return not (ip.is_loopback or ip.is_link_local or ip.is_multicast or ip.is_unspecified)


@dataclass(frozen=True)
class Route:
    """Requests by method and path, as a feature writes them: `POST /api/auth/token`, `GET|PUT /cache/**`, `* /mcp`.

    Methods are separated by `|`; `*` is any. In the path, `*` is any part of
    one segment, `**` any number of whole segments (none included), and
    `{a,b}` either of its alternatives, which may hold slashes. Letters match
    whatever their case.

    A path is matched as the server is likely to route it (_canonical), not
    as the bottle spelled it, so `/api/auth//%74oken;x` is `/api/auth/token`.
    """

    text: str
    methods: frozenset[str]  # uppercase; empty for any
    patterns: tuple[re.Pattern, ...]
    paths: tuple[str, ...] = ()  # the path's, braces expanded, lowercase

    @classmethod
    def parse(cls, text: str) -> "Route":
        """A route as written; ValueError if it isn't one."""
        methods, _, path = text.strip().partition(" ")
        path = path.strip()
        if not methods or not path.startswith("/") or any(c.isspace() for c in path):
            raise ValueError(f"{text!r} isn't METHOD[|METHOD] /path, e.g. POST /api/auth/token")
        names = frozenset(m.upper() for m in methods.split("|"))
        if names != {"*"} and not all(re.fullmatch(r"[A-Z]+", m) for m in names):
            raise ValueError(f"{text!r}: methods are names separated by |, or *")
        paths = tuple(p.lower() for p in _braces_expanded(path))
        return cls(text.strip(), frozenset() if names == {"*"} else names, tuple(_path_pattern(p) for p in paths), paths)

    def matches(self, method: str, path: str) -> bool:
        """Whether a request for `path`, already canonical, by `method`, is one of this route's."""
        return (not self.methods or method.upper() in self.methods) and any(p.fullmatch(path) for p in self.patterns)

    def overlaps_forbidden(self, forbidden: "Route") -> bool:
        """Whether a request could be both one of this route's and one `forbidden` refuses.

        As Injection.forbids refuses it: also with an extension on the last
        segment, taken here as anything more after it, which errs wide.
        """
        if self.methods and forbidden.methods and not self.methods & forbidden.methods:
            return False
        return any(_paths_meet(_path_tokens(mine), _path_tokens(theirs) + suffix)
                   for mine in self.paths for theirs in forbidden.paths for suffix in ([], [".", _STAR]))


# Where a Rewrite finds the stand-in, and how it puts the credential there.
RAW, BASE64 = "raw", "base64"


@dataclass(frozen=True)
class Rewrite:
    """Somewhere other than the injection's header that a tool presents the stand-in, on the requests `route` names.

    In a header whose whole value is the stand-in (`header`), or in a
    top-level string field of a JSON body (`json`), exactly: the credential
    goes there in the same form, nowhere else. BASE64 is the standard
    alphabet; the stand-in is recognised with or without its padding, and the
    credential is sent without, as Java's Base64.getEncoder().withoutPadding()
    writes it.
    """

    route: Route
    header: str = ""
    json: str = ""
    encoding: str = RAW

    def encoded(self, value: str) -> str:
        if self.encoding == BASE64:
            return base64.b64encode(value.encode()).decode().rstrip("=")
        return value

    def presents(self, value: str, standin: str) -> bool:
        """Whether `value`, where this rewrite looks, is the stand-in, in this rewrite's encoding."""
        if self.encoding == BASE64:
            return bool(standin) and value.strip().rstrip("=") == self.encoded(standin)
        return bool(standin) and value.strip() == standin


@dataclass(frozen=True)
class Injection:
    """A credential folio serve attaches on the bottle's behalf, for one host.

    A feature declares which hosts its credential belongs to (see
    features.py); folio serve looks the credential up on the machine and builds these
    per bottle, so the token itself stays outside the bottle.

    The bottle talks to an injected host the way it talks to any other: HTTPS,
    straight to it. TLS is opaque, and a header can only be attached to a
    request folio serve can read, so for this host and port its machine routes the
    flow to folio serve, bottle-cli completing the TLS handshake with a certificate
    from the machine's CA, which the bottle trusts for its credentials' and
    cached hosts only; folio serve reads each request, and sends it
    on over its own verified TLS connection. A credential is never on the wire in the clear, in the bottle
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

    So what the credential may be spent on is said by route. A request on one
    of `forbid`'s never leaves, with the credential or without: those are the
    ones whose answer is a credential (a token minted from it, a key
    provisioned, a runner registered), or that let something outside the
    bottle act with it later. When there are `allow` routes, the credential
    goes only on a request on one of them, or of a rewrite's; a request on
    another that presents the stand-in is refused, so it is plain why it
    failed. Some tools present the stand-in somewhere other than the header
    (`rewrites`): in a JSON body, or encoded in a header of their own.

    An injected host is reachable at a private address (see reachable).
    """

    host: str  # a pattern, e.g. "teamcity.corp.example.com" or "*.example.com"
    header: str
    value: str  # the whole header value, credential included
    port: int = 443
    standin: str = ""  # what a request presents to be given the credential; "" matches none
    credential: str = ""  # the bare credential, for Basic and rewrites; "" when the feature takes neither
    rewrites: tuple[Rewrite, ...] = ()
    forbid: tuple[Route, ...] = ()  # never sent, credential or not
    allow: tuple[Route, ...] = ()  # the only routes the credential goes on, when there are any
    feature: str = ""  # whose credential it is, as a refusal names it

    def forbids(self, method: str, path: str) -> bool:
        """Whether a request for `path` (canonical) is never sent to this host.

        Also with an extension on its last segment, which some servers route
        as the path without it (`/api/auth/token.json`): a refusal errs wide.
        """
        unsuffixed = re.sub(r"\.[^/.]*$", "", path)
        return any(r.matches(method, p) for r in self.forbid for p in {path, unsuffixed})

    @property
    def named(self) -> str:
        """The credential, as a refusal names it: "github credential"."""
        return f"{self.feature} credential" if self.feature else "credential"

    def allows(self, method: str, path: str) -> bool:
        """Whether the credential may go on a request for `path` (canonical)."""
        return not self.allow or any(r.matches(method, path) for r in (*self.allow, *(w.route for w in self.rewrites)))

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

    def __init__(self, status: int | None, reason: str, outcome: str, body: bytes | None = None,
                 content_type: str = "text/plain") -> None:
        super().__init__(reason)
        self.status = status
        self.reason = reason
        self.outcome = outcome
        # What the bottle is told, when it's more than the reason.
        self.body = body
        self.content_type = content_type


@dataclass
class Request:
    """A request inside a credential's host's TLS, made ready for the host: its head (and body, if a rewrite read
    it) as it's sent on, and whether the credential went on it."""

    method: str
    host: str
    port: int
    upstream_head: bytes
    path: str = ""
    injected: bool = False


@dataclass(frozen=True)
class Flow:
    """One connection a bottle's machine routed here, as bottle-cli says (services.read_header): the address the
    guest connected to, the name it asked for (SNI or Host), and `unwrapped` if the bottle spoke TLS, which
    bottle-cli has taken off."""

    address: str
    port: int
    name: str | None = None
    unwrapped: bool = False


# What a flow is for.
GIT, CREDENTIAL, SSH = "git", "credential", "ssh"


@dataclass(frozen=True)
class Redirect:
    """Which of folio serve's services a flow is for: bottle.host's git service, ssh to a bottle, or attaching
    `injection`'s credential, its host reached at `addresses`, already judged."""

    service: str
    injection: Injection | None = None
    addresses: tuple[str, ...] = ()


class Gateway:
    """One bottle's way out: the far end of the flows its machine routes here."""

    def __init__(
        self,
        name: str,
        services: "host.HostServices | None" = None,
        injections: tuple[Injection, ...] = (),
        ssh: "Callable[[str], Awaitable[list[str]]] | None" = None,
        resolve: Resolver | None = None,
        reachable: Callable[[IPAddress], bool] = reachable,
    ) -> None:
        self.name = name
        # bottle.host's services: flows to host.ADDRESS are answered here.
        self.services = services
        # Credentials attached for the bottle, by destination, on what bottle-cli gives here unwrapped.
        self.injections = tuple(injections)
        # The command line that runs sshd -i in the bottle granted at an
        # address, which a flow to it is joined to; raises Refused if none is.
        self.ssh = ssh
        self.resolve = resolve or _resolve
        self.reachable = reachable

    def log_flow(self, target: str, ip: str | None, outcome: str, status: str | None, up: list[int], down: list[int],
             started: float) -> None:
        log.log(
            logging.INFO if outcome == "ok" else logging.WARNING,
            "%s %s -> %s %s%s up=%d down=%d %.2fs",
            self.name, target, ip or "-", outcome, f" {status}" if status else "",
            up[0], down[0], time.monotonic() - started,
        )

    async def decide(self, flow: Flow) -> Redirect:
        """Which service takes `flow`; refused if none does (a policy out of date routed it here).

        Judged by the address the bottle connected to, and by the name it asked
        for, which is the bottle's claim, so it's checked before it opens anything.
        """
        address = ipaddress.ip_address(flow.address)
        if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped:
            address = address.ipv4_mapped  # judged as the IPv4 address it is
        if address == ipaddress.ip_address(host.ADDRESS):
            if flow.port == 80 and self.services is not None:
                return Redirect(GIT)
            raise Refused(403, "Forbidden: no such host service", "denied")
        if address in ipaddress.ip_network(host.SSH_NETWORK):
            # Never anywhere on the network: only ever to sshd in the bottle granted there.
            if flow.port == host.SSH_PORT and self.ssh is not None:
                return Redirect(SSH)
            raise Refused(403, "Forbidden: no such bottle service", "denied")
        # A credential goes only on TLS.
        injection = self._injection_at(flow.name, flow.port) if flow.name and flow.unwrapped else None
        if injection is None:
            raise Refused(None, "not a service of folio serve's", "not-a-service")
        # Sent to the name, never to the address the bottle chose: the credential goes only where its host's
        # name leads.
        return Redirect(CREDENTIAL, injection, tuple(await self._allowed(flow.name, flow.port)))

    async def serve(self, flow: Flow, reader, writer) -> None:
        """Be the far end of `flow`, as decide says."""
        started = time.monotonic()
        name = flow.name
        target = f"{flow.address}:{flow.port}" + (f" ({name})" if name else "")
        ip, up, down, response = None, [0], [0], bytearray()
        outcome, status = "ok", None
        upstream_writer = None
        decision = None
        try:
            decision = await self.decide(flow)
            if decision.service == GIT:
                ip = "host"
                outcome = await self._serve_git(reader, writer)
                return
            if decision.service == SSH:
                ip = "ssh"
                await self._serve_ssh(flow, reader, writer, up, down)
                return
            request = await self._inside(name, flow.port, decision.injection, reader, writer)
            if request.injected:
                target += " +credential"
            ip, upstream_reader, upstream_writer = await self._open(request, list(decision.addresses))
            upstream_writer.write(request.upstream_head)
            up[0] += len(request.upstream_head)
            await _relay(reader, writer, upstream_reader, upstream_writer, up, down, response)
            status = _status(response)
            # Each route, and whether it took the credential: what a feature's
            # allow list is made from. Without the query, which a route ignores
            # and which may carry a signature of its own.
            self._log_request(request.method, name, request.path, "credential" if request.injected else "passed", status)
        except Refused as e:
            outcome = e.outcome
            # Answered in plain http: bottle-cli wraps it for a bottle speaking TLS.
            if e.status is not None and (flow.unwrapped or (decision is not None and decision.service == GIT)):
                await _respond(writer, e.status, e.reason, e.body, e.content_type)
        except Exception:
            outcome = "error"
            log.exception("%s: unexpected error", self.name)
        finally:
            for w in (upstream_writer, writer):
                if w is not None:
                    w.close()
            self.log_flow(target, ip, outcome, status, up, down, started)

    async def _serve_ssh(self, flow: Flow, reader, writer, up: list[int], down: list[int]) -> None:
        """Join the flow to sshd in the bottle granted at its address, until sshd is done."""
        command = await self.ssh(flow.address)
        process = await asyncio.create_subprocess_exec(
            *command, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        said = asyncio.create_task(process.stderr.read())
        try:
            await _relay(reader, writer, process.stdout, process.stdin, up, down)
        finally:
            if process.returncode is None:
                process.kill()
            await process.wait()
            errors = (await said).decode(errors="replace").strip()
            if process.returncode and errors:
                log.warning("%s: sshd at %s: %s", self.name, flow.address, errors.splitlines()[-1])

    async def _serve_git(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> str:
        """Answer an origin-form request to bottle.host."""
        method, target, _, header_lines = await _read_head(reader)
        url = urlsplit(target)
        if not target.startswith("/"):
            raise Refused(400, "Bad Request", "bad-request")
        headers = _parsed(header_lines)
        try:
            body = await host.read_body(reader, headers)
        except (ValueError, asyncio.IncompleteReadError):
            raise Refused(400, "Bad Request", "bad-request") from None
        return await self.services.handle(host.HttpRequest(method, url.path or "/", url.query, headers, body), writer)

    def _injection_at(self, host: str, port: int) -> Injection | None:
        """The credential for this host and port, if one claims it."""
        return next((i for i in self.injections if i.matches(host, port)), None)

    def _log_request(self, method: str, name: str, path: str, outcome: str, status: str | None) -> None:
        log.info("%s %s https://%s%s %s %s", self.name, method, name, path, outcome, status or "-")

    async def _inside(self, name: str, port: int, injection: Injection, reader: asyncio.StreamReader,
                      writer: asyncio.StreamWriter) -> Request:
        """The request a bottle sent inside TLS terminated for host `name`, made ready for the upstream.

        Its head, and its body too when a rewrite reads it for the stand-in.
        """
        method, target, version, header_lines = await _read_head(reader)
        url = urlsplit(target)
        if method == "CONNECT" or (url.scheme and url.scheme != "https") or (url.netloc and url.hostname != name.lower()):
            raise Refused(400, "Bad Request", "bad-request")
        if method.upper() in ECHOING_METHODS:
            # The response would carry the credential back into the bottle.
            raise Refused(405, "Method Not Allowed", "denied")
        path = (url.path or "/") + (f"?{url.query}" if url.query else "")
        route = _canonical(url.path or "/")
        if route is None:
            raise Refused(400, "Bad Request", "bad-request")
        parsed = _parsed(header_lines)
        if injection.forbids(method, route):
            self._log_request(method, name, path, "forbidden", "403")
            raise _route_refused(
                parsed, f"bottle refused {method} {url.path} on {name}: the {injection.named}'s feature "
                "forbids this route, with the credential or without.", "forbidden")
        # The credential, if the bottle presented the stand-in under this name.
        presented = [line.split(":", 1)[1] for line in header_lines
                     if line.split(":", 1)[0].strip().lower() == injection.header.lower()]
        value = next((v for v in map(injection.replacing, presented) if v is not None), None)
        # And where else this route's tool presents it.
        rewrites = [r for r in injection.rewrites if r.route.matches(method, route)] if injection.credential else []
        rewritten = {
            r.header.lower(): f"{r.header}: {r.encoded(injection.credential)}" for r in rewrites
            if r.header and any(r.presents(line.split(":", 1)[1], injection.standin) for line in header_lines
                                if line.split(":", 1)[0].strip().lower() == r.header.lower())
        }
        body, in_body = None, False
        if any(r.json for r in rewrites):
            body = await _request_body(reader, writer, parsed)
            body, in_body = _json_with_credential(body, [r for r in rewrites if r.json], injection)
        if (value is not None or rewritten or in_body) and not injection.allows(method, route):
            self._log_request(method, name, path, "not-allowed", "403")
            raise _route_refused(
                parsed, f"bottle refused {method} {url.path} on {name}: the {injection.named} goes only on "
                "the routes its feature allows, and this isn't one of them.", "not-allowed")
        # The Host header goes with the destination, and the routing headers
        # go altogether, so a request carrying a credential can't ask one
        # server to answer as another, or for something else. Nothing else
        # differs between the requests that carry it and those that don't.
        dropped = {"host", *ROUTING_HEADERS} | ({injection.header.lower()} if value is not None else set()) | set(rewritten)
        if body is not None:
            dropped |= {"content-length", "transfer-encoding", "expect"}  # read here, and sent whole
        headers = [line for line in _forwarded(header_lines) if line.split(":", 1)[0].strip().lower() not in dropped]
        authority = name + (f":{port}" if port != 443 else "")
        headers = [f"Host: {authority}", *headers, *rewritten.values()]
        if value is not None:
            headers.append(f"{injection.header}: {value}")
        if body is not None:
            headers.append(f"Content-Length: {len(body)}")
        # A WebSocket (Develocity's test distribution) asks to keep the connection, as something else.
        upgrade = "upgrade" in parsed and "upgrade" in _tokens(parsed.get("connection", ""))
        return Request(
            method, name, port,
            _upstream_head(method, path, version, headers, "Upgrade" if upgrade else "close") + (body or b""),
            url.path or "/", injected=value is not None or bool(rewritten) or in_body,
        )

    async def _allowed(self, host: str, port: int) -> list[str]:
        """The addresses `host` resolves to that a credential's host may be at (reachable); refused if none."""
        try:
            addresses = await self.resolve(host, port)
        except OSError:
            raise Refused(502, "Bad Gateway: cannot resolve host", "unresolved") from None
        allowed = [a for a in addresses if self.reachable(ipaddress.ip_address(a.partition("%")[0]))]
        if not allowed:
            raise Refused(403, "Forbidden: destination not allowed", "denied")
        return allowed

    async def _open(self, request: Request, allowed: list[str]) -> tuple[str, asyncio.StreamReader, asyncio.StreamWriter]:
        """Connect to the request's host, at the addresses already judged (`allowed`), so a second lookup can't
        swap them; always over TLS: a credential is never put on a request that leaves here in the clear, and one
        without it was TLS when the bottle sent it."""
        host, port = request.host, request.port
        context = _tls_context()
        for address in allowed:
            try:
                connect = asyncio.open_connection(address, port, ssl=context, server_hostname=host)
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


def _upstream_head(method: str, path: str, version: str, headers: list[str], connection: str = "close") -> bytes:
    # One request per connection keeps relaying simple: the upstream closes when done.
    return "\r\n".join([f"{method} {path} {version}", *headers, f"Connection: {connection}", "", ""]).encode("latin-1")


async def _request_body(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, headers: dict[str, str]) -> bytes:
    """All of a request's body, by its Content-Length or chunked; refused past MAX_BODY_BYTES, or when it isn't all there."""
    if "100-continue" in _tokens(headers.get("expect", "")):
        # The bottle waits to be told to send it, and it's read here, not by the server.
        writer.write(b"HTTP/1.1 100 Continue\r\n\r\n")
        await writer.drain()

    async def read() -> bytes:
        if "transfer-encoding" in headers:
            if _tokens(headers["transfer-encoding"]) != {"chunked"}:
                raise ValueError("a transfer coding other than chunked")
            body = bytearray()
            while size := int((await reader.readuntil(b"\n")).split(b";")[0].strip(), 16):
                if len(body) + size > MAX_BODY_BYTES:
                    raise Refused(413, "Content Too Large", "bad-request")
                body += await reader.readexactly(size)
                if (await reader.readuntil(b"\n")).strip():
                    raise ValueError("a chunk longer than it said")
            while (await reader.readuntil(b"\n")).strip():  # trailers, until the empty line
                pass
            return bytes(body)
        length = headers.get("content-length", "0")
        if not length.isdigit():
            raise ValueError("a Content-Length that isn't one")
        if int(length) > MAX_BODY_BYTES:
            raise Refused(413, "Content Too Large", "bad-request")
        return await reader.readexactly(int(length))

    try:
        return await asyncio.wait_for(read(), HEADER_TIMEOUT)
    except (ValueError, asyncio.IncompleteReadError, asyncio.LimitOverrunError, TimeoutError, ConnectionError, ssl.SSLError):
        raise Refused(400, "Bad Request", "bad-request") from None


def _json_with_credential(body: bytes, rewrites: list[Rewrite], injection: Injection) -> tuple[bytes, bool]:
    """`body` with the credential in each rewrite's field that holds the stand-in, and whether one did.

    Only a JSON object's own fields, and only one that is exactly the stand-in:
    the stand-in anywhere else in the body stays as it is.
    """
    try:
        document = json.loads(body)
    except ValueError:  # UnicodeDecodeError is one
        return body, False
    if not isinstance(document, dict):
        return body, False
    fields = [r for r in rewrites if isinstance(document.get(r.json), str) and r.presents(document[r.json], injection.standin)]
    for r in fields:
        document[r.json] = r.encoded(injection.credential)
    return (json.dumps(document, ensure_ascii=False, separators=(",", ":")).encode() if fields else body), bool(fields)


def _canonical(path: str) -> str | None:
    """A request's path as a server is likely to route it, lowercase; None if it should go to none.

    Escapes are decoded until there are none left, since a server, or one in
    front of it, may decode twice; a backslash is a slash; a segment's
    parameters (`;jsessionid=...`, which Java servers route without) are
    dropped; and empty segments, `.` and `..` are resolved. Erring wide is
    the point: a route is only ever refused on this, never opened.
    """
    for _ in range(4):
        decoded = unquote(path)
        if decoded == path:
            break
        path = decoded
    else:
        return None
    if any(ord(c) < 0x20 or ord(c) == 0x7F for c in path):
        return None
    segments: list[str] = []
    for segment in path.replace("\\", "/").split("/"):
        segment = segment.partition(";")[0]
        if segment == "..":
            if segments:
                segments.pop()
        elif segment not in ("", "."):
            segments.append(segment)
    return "/" + "/".join(segments).lower()


def _braces_expanded(path: str) -> list[str]:
    """A route's path as the paths its `{a,b}` groups stand for: `/{a,b}/c` is `/a/c` and `/b/c`."""
    group = re.search(r"\{([^{}]*)\}", path)
    if group is None:
        if "{" in path or "}" in path:
            raise ValueError(f"{path!r}: a brace without its pair")
        return [path]
    return [expanded for alternative in group.group(1).split(",")
            for expanded in _braces_expanded(path[:group.start()] + alternative + path[group.end():])]


# A route's path as _path_pattern reads it, for telling whether two meet:
# each a character, or `*` (any characters but a slash) or `**` (any number of
# slash-led segments, as _path_pattern's `(?:/[^/]*)*`).
_STAR, _SEGMENTS = object(), object()


def _path_tokens(path: str) -> list:
    tokens: list = []
    for segment in path.split("/"):
        if segment == "**":
            tokens.append(_SEGMENTS)
        elif segment:
            tokens.append("/")
            for i, part in enumerate(segment.split("*")):
                tokens += ([_STAR] if i else []) + list(part)
    return tokens or ["/"]


def _paths_meet(a: list, b: list) -> bool:
    """Whether some path matches both token lists: a walk of the two automata in step."""
    # A state is (index, inside): `inside` is within a `**`, past one of its slashes.
    def moves(tokens, state):
        """(the character read: itself, "[^/]" for any but a slash, or None for none; the state after)."""
        i, inside = state
        if inside:
            return [(None, (i, False)), ("[^/]", (i, True))]
        if i == len(tokens):
            return []
        token = tokens[i]
        if token is _STAR:
            return [(None, (i + 1, False)), ("[^/]", (i, False))]
        if token is _SEGMENTS:
            return [(None, (i + 1, False)), ("/", (i, True))]
        return [(token, (i + 1, False))]

    def meet(x, y):
        if x == "[^/]" and y == "[^/]":
            return True
        if x == "[^/]":
            return y != "/"
        if y == "[^/]":
            return x != "/"
        return x == y

    start, seen = ((0, False), (0, False)), set()
    stack = [start]
    while stack:
        state = stack.pop()
        if state in seen:
            continue
        seen.add(state)
        (sa, sb) = state
        if sa == (len(a), False) and sb == (len(b), False):
            return True
        for ca, na in moves(a, sa):
            if ca is None:
                stack.append((na, sb))
                continue
            for cb, nb in moves(b, sb):
                if cb is None:
                    stack.append((sa, nb))
                elif meet(ca, cb):
                    stack.append((na, nb))
        for cb, nb in moves(b, sb):
            if cb is None:
                stack.append((sa, nb))
    return False


def _path_pattern(path: str) -> re.Pattern:
    """A route's path, without braces, as a regular expression over canonical paths."""
    regex = ""
    for segment in path.split("/"):
        if segment == "**":
            regex += "(?:/[^/]*)*"
        elif segment:
            regex += "/" + "[^/]*".join(re.escape(part) for part in segment.split("*"))
    return re.compile(regex or "/", re.IGNORECASE)


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


def _route_refused(headers: dict[str, str], why: str, outcome: str) -> Refused:
    """A 403 for a route a credential's feature refuses, saying so to whoever reads it: often an agent.

    As JSON when the request asked for it, in the fields API clients show
    (`message`, as GitHub's errors have it, and Anthropic's `error.message`);
    else as text, which git shows as the remote's message.
    """
    message = (
        f"{why} This is the bottle's policy, not an outage or a bug: don't retry the request, or look for another "
        "way to make it. If your task needs this route, stop and tell the user which request was refused and why "
        "you need it: only they can allow it."
    )
    if "json" in headers.get("accept", "") or "json" in headers.get("content-type", ""):
        body = json.dumps({"type": "error", "error": {"type": "permission_error", "message": message},
                           "message": message}).encode() + b"\n"
        return Refused(403, "Forbidden", outcome, body, "application/json")
    return Refused(403, "Forbidden", outcome, f"{message}\n".encode())


async def _respond(writer: asyncio.StreamWriter, status: int, reason: str, body: bytes | None = None,
                   content_type: str = "text/plain") -> None:
    body = body if body is not None else f"{reason}\n".encode()
    writer.write(
        f"HTTP/1.1 {status} {reason.split(':')[0]}\r\n"
        f"Content-Type: {content_type}\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode()
        + body
    )
    try:
        await writer.drain()
    except (ConnectionError, OSError):
        pass
