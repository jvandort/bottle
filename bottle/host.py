"""Host services: what a bottle can ask of the host, at bottle.host.

Requests to http://bottle.host/ never leave the host: the decider hands them
to bottled, which answers them itself, serving the bottle's repo over git's
smart HTTP protocol with `git http-backend`. A bottle is served the remotes it
was created with, each a Remote at http://bottle.host/git/<name>. What a fetch
sees is one of three views:

  upstream
      The repo's upstream: the host repo's refs/remotes/origin/*, served as
      branches (so `git checkout release` works in the bottle), plus tags.
      Pull-through: before serving, the host runs `git fetch origin` in the
      repo (throttled, never prompting), so the bottle gets the real upstream's
      latest even if nobody fetched on the host.

  heads
      The host repo's own refs, as they are: what the bottle reads to build on
      what the user has (`git rebase host/main`).

  ns/<namespace>
      The refs in one git namespace (gitnamespaces(7)) of the host repo, and
      nothing else.

A remote that takes pushes writes one namespace. A namespace is what confines
the push -- every ref receive-pack resolves is inside it, and the host's own
refs aren't advertised -- so git behaves like git in the bottle while being
unable to name anything else. Pushes are also append-only, refusing
force-pushes and deletes, so what someone has already read never changes
underneath them. bottle's hooks/pre-receive only refuses a push that arrives
with no namespace at all; the repo's own hooks never run.

Reading one namespace and writing another makes a two-lane exchange, each lane
with one writer, so neither side's push is ever rejected for what the other
did: the bottle writes one lane, and whoever created it writes the other,
directly in its own repo. bottled never serves that direction.

Nothing is mirrored or watched: every request sees the repo as it is right now.

After a push, the host's checked-out branches follow it (hooks/follow.py):
one whose upstream is a remote reading the push's namespace (folio's
bottle/<thread>) is fast-forwarded to the bottle's branch of the same name,
files and all, in whichever worktree has it, unless the repo says
`git config bottle.follow false`. Only ever a fast-forward, and only when it can't lose anything: a branch with commits of
the host's own, a change in the way, or a worktree mid-merge, -rebase and the
like is left alone. The push succeeds either way, and its output tells the
bottle what moved and what didn't; bottled logs it too.

A bottle can also send a message to another bottle: a POST to
http://bottle.host/messages of {"to": ..., "text": ...}, where to is an address
it was given (see messages.py) or, with the message-any permission, a running
bottle's name. bottled hands it to that bottle's session as an event from the
sender (see Daemon.deliver), and answers once the session has taken it, or
with why it couldn't: no running bottle it may reach by that (404), or no
session took it in time (504). The sender names where a message goes, never
where it came from: bottled knows which bottle is asking, and the event's from
is its address, issued to the recipient.

With the list-bottles permission, GET http://bottle.host/bottles lists the
other running bottles, as JSON: each one's name, repo and thread.
"""

import asyncio
import hashlib
import io
import json
import logging
import os
import re
import subprocess
import sys
import tempfile
import time
import zlib
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from http.client import parse_headers
from pathlib import Path

from bottle.errors import BottleError

log = logging.getLogger("bottle.egress")

HOST = "bottle.host"
# Where a bottle whose driver carries its sockets reaches bottle.host: its
# /etc/hosts says so. A documentation address (TEST-NET-1) that nothing real
# uses, which smolvm hands to the decider like any other.
ADDRESS = "192.0.2.1"
GIT_PATH = "/git"
MESSAGES_PATH = "/messages"
BOTTLES_PATH = "/bottles"
MAX_BODY = 64 * 1024 * 1024
MAX_MESSAGE = 1024 * 1024
HOOKS = Path(__file__).resolve().parent / "hooks"


# What a remote's fetch can see: the upstream (served as origin is), the repo's
# own branches (as host is), or one namespace, NS + its name.
UPSTREAM, HEADS, NS = "upstream", "heads", "ns/"
# Remote and namespace names: one path segment, so a remote is one URL segment
# and a namespace is flat. Namespaces nest rather than concatenate, so `a/b`
# would mean refs/namespaces/a/refs/namespaces/b/, not what anyone expects.
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


@dataclass(frozen=True)
class Remote:
    """One remote a bottle is served: http://bottle.host/git/<name>."""

    name: str
    fetch: str  # UPSTREAM, HEADS, or NS + a namespace
    push: str | None = None  # NS + the namespace pushes go into; None: read-only

    @property
    def fetch_namespace(self) -> str | None:
        return self.fetch.removeprefix(NS) if self.fetch.startswith(NS) else None

    @property
    def push_namespace(self) -> str | None:
        return self.push.removeprefix(NS) if self.push else None

    @property
    def spec(self) -> str:
        """As `--git-remote` takes it, less its mount: work:fetch=ns/x-in,push=ns/x-out."""
        return f"{self.name}:fetch={self.fetch}" + (f",push={self.push}" if self.push else "")

    @classmethod
    def parse(cls, spec: str) -> "Remote":
        """A remote from its spec: NAME:fetch=VIEW[,push=ns/NAMESPACE]."""
        usage = (f"invalid remote {spec!r}: use NAME:fetch=VIEW[,push=ns/NAMESPACE], "
                 f"VIEW being {UPSTREAM}, {HEADS} or ns/NAMESPACE")
        name, colon, rest = spec.partition(":")
        if not colon or not NAME.fullmatch(name):
            raise BottleError(usage)
        settings: dict[str, str] = {}
        for item in rest.split(","):
            key, equals, value = item.partition("=")
            if not equals or key not in ("fetch", "push") or key in settings:
                raise BottleError(usage)
            settings[key] = value
        fetch, push = settings.get("fetch"), settings.get("push")
        if fetch is None or not (fetch in (UPSTREAM, HEADS) or _namespace_view(fetch)):
            raise BottleError(usage)
        if push is not None and not _namespace_view(push):
            raise BottleError(f"invalid remote {spec!r}: push takes ns/NAMESPACE, the namespace pushes go into")
        return cls(name, fetch, push)


def _namespace_view(view: str) -> bool:
    return view.startswith(NS) and NAME.fullmatch(view.removeprefix(NS)) is not None

# At most one pull-through fetch per repo per this many seconds.
PULL_THROUGH_INTERVAL = 60
PULL_THROUGH_TIMEOUT = 120


@dataclass
class HttpRequest:
    method: str
    path: str  # without the query
    query: str
    headers: dict[str, str]  # lowercase names
    body: bytes = b""


class ServiceError(Exception):
    """A request bottled refused or couldn't carry out, with the HTTP status that says why."""

    def __init__(self, status: int, reason: str, why: str) -> None:
        super().__init__(why)
        self.status, self.reason = status, reason


@dataclass(frozen=True)
class Served:
    """One repo a bottle is served -- a mount's -- with its remotes, at http://bottle.host/git/<at>/<remote>."""

    git_dir: Path | None  # the repo its remotes are served from; None: it's gone
    remotes: tuple[Remote, ...] = ()
    # Where under /git its remotes are: the mount's path, without the leading /.
    # "" puts them at /git/<remote>, as bottles made before mounts have them.
    at: str = ""
    view_dir: Path | None = None  # where its upstream view is written (default: under BOTTLE_HOME)

    def served(self) -> dict[str, Remote]:
        return {remote.name: remote for remote in self.remotes}


@dataclass
class HostServices:
    """What one bottle may ask of the host."""

    repos: tuple[Served, ...] = ()  # the repos it's served, each a mount's
    bottle: str | None = None  # who's asking: the sender of its messages
    # Hands a message to a bottle: (sender, to, text); raises ServiceError. None takes no messages.
    deliver: Callable[[str, str, str], Awaitable[None]] | None = None
    # The running bottles the asker may see: (asker) -> [{name, repo, thread}]; raises ServiceError.
    peers: Callable[[str], Awaitable[list[dict]]] | None = None

    def route(self, path: str) -> tuple[Served, Remote, str] | None:
        """The repo, remote and rest of path a /git/... request is for; None if none is served there."""
        if not path.startswith(GIT_PATH + "/"):
            return None
        rest = path[len(GIT_PATH) + 1:]
        # The longest `at` first, so a mount at /a/b isn't taken for one at /a.
        for repo in sorted(self.repos, key=lambda r: len(r.at), reverse=True):
            prefix = f"{repo.at}/" if repo.at else ""
            if repo.git_dir is None or not rest.startswith(prefix):
                continue
            name, _, path_info = rest[len(prefix):].partition("/")
            remote = repo.served().get(name)
            if remote is not None:
                return repo, remote, "/" + path_info
        return None

    async def handle(self, request: HttpRequest, writer: asyncio.StreamWriter) -> str:
        """Answer a request to bottle.host; returns an outcome for the log."""
        routed = self.route(request.path)
        if routed is not None:
            return await self._git(*routed, request, writer)
        if self.bottle is not None and self.deliver is not None and request.path == MESSAGES_PATH:
            return await self._message(request, writer)
        if self.bottle is not None and self.peers is not None and request.path == BOTTLES_PATH:
            return await self._bottles(request, writer)
        await _respond(writer, 404, "Not Found", b"no such bottle.host service\n")
        return "not-found"

    async def _message(self, request: HttpRequest, writer: asyncio.StreamWriter) -> str:
        if request.method != "POST":
            await _respond(writer, 405, "Method Not Allowed", b"POST a message: {\"to\": ..., \"text\": ...}\n")
            return "bad-message"
        problem = _message_problem(request.body)
        if problem:
            await _respond(writer, 400, "Bad Request", f"{problem}\n".encode())
            return "bad-message"
        message = json.loads(request.body)
        try:
            await self.deliver(self.bottle, message["to"], message["text"])
        except ServiceError as e:
            await _respond(writer, e.status, e.reason, f"{e}\n".encode())
            return "undelivered"
        await _respond(writer, 200, "OK", b"delivered\n")
        return "message"

    async def _bottles(self, request: HttpRequest, writer: asyncio.StreamWriter) -> str:
        if request.method != "GET":
            await _respond(writer, 405, "Method Not Allowed", b"GET the running bottles\n")
            return "bad-request"
        try:
            peers = await self.peers(self.bottle)
        except ServiceError as e:
            await _respond(writer, e.status, e.reason, f"{e}\n".encode())
            return "refused"
        await _respond(writer, 200, "OK", json.dumps(peers).encode() + b"\n", "application/json")
        return "bottles"

    async def _git(self, repo: Served, remote: Remote, path_info: str, request: HttpRequest,
                   writer: asyncio.StreamWriter) -> str:
        git_dir = repo.git_dir
        assert git_dir is not None                  # route() serves no repo without one
        push = "git-receive-pack" in path_info or "service=git-receive-pack" in request.query
        if push and remote.push is None:
            writable = [r.name for r in repo.remotes if r.push is not None]
            hint = f"; push to {' or '.join(writable)} instead" if writable else ""
            await _respond(writer, 403, "Forbidden", f"read-only{hint}\n".encode())
            return "denied"
        config: dict[str, str] = {}
        namespace = None
        if not push and remote.fetch == UPSTREAM:
            if "info/refs" in path_info:  # once per fetch, at its start
                await pull_through(git_dir)
            root = await asyncio.to_thread(write_view, git_dir, repo.view_dir or default_view_dir(git_dir))
        else:
            root = git_dir
            # A fetch reads its namespace, if it has one; a push writes its own.
            namespace = remote.push_namespace if push else remote.fetch_namespace
            if push:
                config = {
                    "http.receivepack": "true",
                    "core.hooksPath": str(HOOKS),  # bottle's rules; the repo's own hooks never run
                    "receive.fsckObjects": "true",
                    # Nothing a reviewer has read may change underneath them, so
                    # the out lane is append-only. A bottle with a stream to
                    # abandon pushes a new branch; rewriting is the host's, in
                    # its own repo, where every git command is available.
                    "receive.denyNonFastForwards": "true",
                    "receive.denyDeletes": "true",
                    "core.logAllRefUpdates": "always",  # refs outside refs/heads get no reflog by default
                    # A bottle pushing `main` writes its lane's main, but git
                    # compares the unnamespaced name against the host's
                    # checked-out branch and would refuse it.
                    "receive.denyCurrentBranch": "ignore",
                }
        # What hooks/post-receive needs to make the host's branches follow a push, and where it says what it did.
        hook_log = None
        if push and request.method == "POST":
            hook_log = Path(tempfile.mkstemp(prefix="bottle-follow-", suffix=".log")[1])
        body = request.body
        if request.headers.get("content-encoding") == "gzip":
            body = zlib.decompress(body, 16 + zlib.MAX_WBITS)
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": os.environ.get("HOME", "/"),
            "GIT_PROJECT_ROOT": str(root),
            "GIT_HTTP_EXPORT_ALL": "1",
            "PATH_INFO": path_info,
            "REQUEST_METHOD": request.method,
            "QUERY_STRING": request.query,
            "CONTENT_TYPE": request.headers.get("content-type", ""),
            "CONTENT_LENGTH": str(len(body)),
            "REMOTE_ADDR": "bottle",
            "REMOTE_USER": "bottle",
            # What confines a push: every ref receive-pack resolves is under
            # refs/namespaces/<this>/. hooks/pre-receive refuses a push without
            # one, so never pass an empty one.
            **({"GIT_NAMESPACE": namespace} if namespace else {}),
            **({"GIT_PROTOCOL": request.headers["git-protocol"]} if "git-protocol" in request.headers else {}),
            **_config_env(config),
            **({"BOTTLE_PYTHON": sys.executable, "BOTTLE_HOOK_LOG": str(hook_log)} if hook_log is not None else {}),
        }
        process = await asyncio.create_subprocess_exec(
            "git", "http-backend", env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        out, err = await process.communicate(body)
        if process.returncode != 0 and not out:
            log.warning("git http-backend failed: %s", err.decode(errors="replace").strip())
            await _respond(writer, 500, "Internal Server Error", b"git http-backend failed\n")
            return "failed"
        if hook_log is not None:
            for line in hook_log.read_text().splitlines() if hook_log.exists() else ():
                log.info("%s: %s", git_dir, line)
            hook_log.unlink(missing_ok=True)
        await _relay_cgi(out, writer)
        return "pushed" if push else "ok"


_last_pull: dict[Path, float] = {}
_pull_locks: dict[Path, asyncio.Lock] = {}


def _message_problem(body: bytes) -> str | None:
    """What's wrong with a message's body, or None: it's {"to": a destination, "text": the message}."""
    if len(body) > MAX_MESSAGE:
        return f"a message is at most {MAX_MESSAGE} bytes"
    try:
        message = json.loads(body)
    except ValueError:
        return 'a message is JSON: {"to": ..., "text": ...}'
    if not isinstance(message, dict):
        return 'a message is a JSON object: {"to": ..., "text": ...}'
    extra = sorted(set(message) - {"to", "text"})
    if extra:
        return f"a message has only to and text, not {', '.join(extra)}"
    if not isinstance(message.get("to"), str) or not message["to"].strip():
        return "a message needs to: the address or bottle it's for"
    if not isinstance(message.get("text"), str):
        return "a message needs text"
    return None


async def pull_through(git_dir: Path) -> None:
    """`git fetch origin` in the host repo, at most every PULL_THROUGH_INTERVAL seconds.

    Uses the host's network and credentials, never prompts, and never fails the
    bottle's request: if it can't fetch, the bottle gets what the repo has.
    """
    async with _pull_locks.setdefault(git_dir, asyncio.Lock()):
        if time.monotonic() - _last_pull.get(git_dir, float("-inf")) < PULL_THROUGH_INTERVAL:
            return
        _last_pull[git_dir] = time.monotonic()
        env = {**os.environ, "GIT_TERMINAL_PROMPT": "0", "GIT_SSH_COMMAND": "ssh -o BatchMode=yes"}
        try:
            result = await asyncio.to_thread(
                subprocess.run, ["git", "--git-dir", str(git_dir), "fetch", "--quiet", "origin"],
                env=env, capture_output=True, text=True, timeout=PULL_THROUGH_TIMEOUT,
            )
        except subprocess.TimeoutExpired:
            log.warning("pull-through: git fetch origin in %s timed out", git_dir)
            return
        if result.returncode != 0:
            log.warning("pull-through: git fetch origin in %s failed: %s", git_dir, result.stderr.strip())


def _config_env(config: dict[str, str]) -> dict[str, str]:
    """git config as environment variables (GIT_CONFIG_COUNT and friends)."""
    env = {"GIT_CONFIG_COUNT": str(len(config))} if config else {}
    for i, (key, value) in enumerate(config.items()):
        env[f"GIT_CONFIG_KEY_{i}"] = key
        env[f"GIT_CONFIG_VALUE_{i}"] = value
    return env


def default_view_dir(git_dir: Path) -> Path:
    from bottle.store import bottle_home

    return bottle_home() / "views" / f"{hashlib.sha256(str(git_dir).encode()).hexdigest()[:16]}.git"


def write_view(git_dir: Path, view: Path) -> Path:
    """(Re)write the origin view of `git_dir` at `view`, from the repo's refs right now; return `view`.

    The view is one persistent directory per repo (default_view_dir), updated
    in place before each origin fetch: a tiny bare repo of four files. Its
    objects are the repo's own, via objects/info/alternates, so nothing is
    copied; only packed-refs (the repo's refs/remotes/origin/*, as branches,
    plus tags) really changes.
    """
    out = subprocess.run(
        ["git", "--git-dir", str(git_dir), "for-each-ref", "--format=%(objectname) %(refname)",
         "refs/remotes/origin", "refs/tags"],
        capture_output=True, text=True, check=True,
    ).stdout
    refs = [tuple(line.split(" ", 1)) for line in out.splitlines()]
    view_refs = [(sha, "refs/heads/" + ref.removeprefix("refs/remotes/origin/")) for sha, ref in refs
                 if ref.startswith("refs/remotes/origin/") and ref != "refs/remotes/origin/HEAD"]
    view_refs += [(sha, ref) for sha, ref in refs if ref.startswith("refs/tags/")]
    names = {ref for _, ref in view_refs}
    head = next((r for r in ("refs/heads/main", "refs/heads/master") if r in names), "refs/heads/main")

    (view / "objects" / "info").mkdir(parents=True, exist_ok=True)
    (view / "refs").mkdir(exist_ok=True)
    _write(view / "config", "[core]\n\trepositoryformatversion = 0\n\tbare = true\n")
    _write(view / "objects" / "info" / "alternates", f"{git_dir / 'objects'}\n")
    _write(view / "HEAD", f"ref: {head}\n")
    _write(view / "packed-refs", "# pack-refs with: sorted \n" + "".join(
        f"{sha} {ref}\n" for sha, ref in sorted(view_refs, key=lambda r: r[1])))
    return view


def _write(path: Path, text: str) -> None:
    """Replace `path` atomically: write a temporary file beside it, then rename it
    over `path`, so a concurrent request reads the whole old file or the whole new one."""
    staging = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    staging.write_text(text)
    staging.replace(path)


async def read_body(reader: asyncio.StreamReader, headers: dict[str, str]) -> bytes:
    """A request body, by Content-Length or chunked transfer encoding."""
    if headers.get("transfer-encoding", "").lower() == "chunked":
        chunks = []
        total = 0
        while True:
            size = int((await reader.readline()).split(b";")[0].strip() or b"0", 16)
            if size == 0:
                await reader.readline()  # the empty trailer line
                return b"".join(chunks)
            total += size
            if total > MAX_BODY:
                raise ValueError("request body too large")
            chunks.append(await reader.readexactly(size))
            await reader.readline()  # CRLF after each chunk
    length = int(headers.get("content-length", "0") or 0)
    if length > MAX_BODY:
        raise ValueError("request body too large")
    return await reader.readexactly(length) if length else b""


# Framing headers this relay sets itself, so a copy from the CGI program would
# duplicate them. `Status` is the CGI way of setting the HTTP status line and
# becomes one here, so it is not forwarded as a header either.
CGI_DROP = {"status", "content-length", "connection", "transfer-encoding"}


async def _relay_cgi(output: bytes, writer: asyncio.StreamWriter) -> None:
    """Turn a CGI response (headers, blank line, body) into an HTTP one.

    parse_headers reads the header block -- both CRLF and bare LF, and folded
    continuation lines -- and stops at the blank line, so what is left in the
    stream is the body, byte for byte, packfile and all. Better than splitting
    by hand: the body is never re-parsed, and header casing survives.
    """
    stream = io.BytesIO(output)
    message = parse_headers(stream)
    body = stream.read()
    status = (message.get("Status") or "200 OK").strip()
    headers = "".join(f"{name}: {value}\r\n" for name, value in message.items()
                      if name.lower() not in CGI_DROP)
    writer.write(f"HTTP/1.1 {status}\r\n{headers}".encode("latin-1"))
    writer.write(f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body)
    await writer.drain()


async def _respond(writer: asyncio.StreamWriter, status: int, reason: str, body: bytes,
                   content_type: str = "text/plain") -> None:
    writer.write(
        f"HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len(body)}\r\n"
        f"Connection: close\r\n\r\n".encode() + body
    )
    await writer.drain()
