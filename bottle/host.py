"""Host services: what a bottle can ask of the host, through its egress proxy.

Requests to http://bottle.host/ never leave the host: the bottle's egress proxy
answers them itself, serving the bottle's repo over git's smart HTTP protocol
with `git http-backend`. A bottle has three remotes, each exactly what it says:

  origin  http://bottle.host/git/origin
      The repo's upstream: the host repo's refs/remotes/origin/*, served as
      branches (so `git checkout release` works in the bottle), plus tags.
      Read-only. Pull-through: before serving, the host runs `git fetch origin`
      in the repo (throttled, never prompting), so the bottle gets the real
      upstream's latest even if nobody fetched on the host.

  host    http://bottle.host/git/host
      The host repo's own branches. Read-only: the bottle reads them to build on
      what the user has (`git rebase host/main`) and writes nothing here.

  work    http://bottle.host/git/work
      The bottle's half of a two-lane exchange with the host, in git namespaces
      (gitnamespaces(7)). Each lane has exactly one writer, so neither side's
      push is ever rejected for what the other did:

          fetch  GIT_NAMESPACE=bottle-<name>-in    written by the host
          push   GIT_NAMESPACE=bottle-<name>-out   written by the bottle

      A namespace is what confines the push -- every ref receive-pack resolves
      is inside it, and the host's own refs aren't advertised -- so git behaves
      like git in the bottle while being unable to name anything else. The out
      lane also refuses force-pushes and deletes, so what a reviewer has already
      read never changes underneath them. bottle's hooks/pre-receive only
      refuses a push that arrives with no namespace at all; the repo's own hooks
      never run.

      The host writes the in lane itself, through an ordinary git remote in its
      own repo; bottled never serves that direction.

Nothing is mirrored or watched: every request sees the repo as it is right now.
"""

import asyncio
import hashlib
import io
import logging
import os
import subprocess
import time
import zlib
from dataclasses import dataclass
from http.client import parse_headers
from pathlib import Path

log = logging.getLogger("bottle.egress")

HOST = "bottle.host"
GIT_PATH = "/git"
ORIGIN, HOST_REMOTE, WORK_REMOTE = "origin", "host", "work"
OUT, IN = "out", "in"
MAX_BODY = 64 * 1024 * 1024
HOOKS = Path(__file__).resolve().parent / "hooks"


def lane(bottle: str, direction: str) -> str:
    """The git namespace for one direction of a bottle's exchange with its host.

    Flat by necessity: namespaces nest rather than concatenate, so `a/b` would
    mean refs/namespaces/a/refs/namespaces/b/ and not what anyone expects.
    """
    return f"bottle-{bottle}-{direction}"
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


@dataclass
class HostServices:
    """What one bottle may ask of the host."""

    git_dir: Path | None = None  # the repo served as origin and host, or None for no repo
    view_dir: Path | None = None  # where the origin view is written (default: under BOTTLE_HOME)
    bottle: str | None = None  # whose git namespace pushes are written into

    async def handle(self, request: HttpRequest, writer: asyncio.StreamWriter) -> str:
        """Answer a request to bottle.host; returns an outcome for the log."""
        remote, _, path_info = request.path.removeprefix(GIT_PATH + "/").partition("/")
        known = (ORIGIN, HOST_REMOTE, WORK_REMOTE)
        if self.git_dir is not None and request.path.startswith(GIT_PATH + "/") and remote in known:
            return await self._git(remote, "/" + path_info, request, writer)
        await _respond(writer, 404, "Not Found", b"no such bottle.host service\n")
        return "not-found"

    async def _git(self, remote: str, path_info: str, request: HttpRequest, writer: asyncio.StreamWriter) -> str:
        push = "git-receive-pack" in path_info or "service=git-receive-pack" in request.query
        if push and remote != WORK_REMOTE:
            await _respond(writer, 403, "Forbidden", b"read-only; push to work instead\n")
            return "denied"
        config: dict[str, str] = {}
        namespace = None
        if remote == ORIGIN:
            if not push and "info/refs" in path_info:  # once per fetch, at its start
                await pull_through(self.git_dir)
            root = await asyncio.to_thread(write_view, self.git_dir, self.view_dir or default_view_dir(self.git_dir))
        else:
            root = self.git_dir
            if remote == WORK_REMOTE:
                # Read the lane the host writes; write the lane this bottle writes.
                namespace = lane(self.bottle, OUT if push else IN) if self.bottle else None
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
        await _relay_cgi(out, writer)
        return "pushed" if push else "ok"


_last_pull: dict[Path, float] = {}
_pull_locks: dict[Path, asyncio.Lock] = {}


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


async def _respond(writer: asyncio.StreamWriter, status: int, reason: str, body: bytes) -> None:
    writer.write(
        f"HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {len(body)}\r\n"
        f"Connection: close\r\n\r\n".encode() + body
    )
    await writer.drain()
