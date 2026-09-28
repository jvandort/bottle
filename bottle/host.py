"""Host services: what a bottle can ask of the host, through its egress proxy.

Requests to http://bottle.host/ never leave the host: the bottle's egress proxy
answers them itself, serving the bottle's repo over git's smart HTTP protocol
with `git http-backend`. A bottle has two remotes, each exactly what it says:

  origin  http://bottle.host/git/origin
      The repo's upstream: the host repo's refs/remotes/origin/*, served as
      branches (so `git checkout release` works in the bottle), plus tags.
      Read-only. Pull-through: before serving, the host runs `git fetch origin`
      in the repo (throttled, never prompting), so the bottle gets the real
      upstream's latest even if nobody fetched on the host.

  host    http://bottle.host/git/host
      The host repo itself: its local branches. The bottle may fetch anything,
      and push only to agent/* branches, creating them or moving them forward.
      That's enforced on the host: `git receive-pack` runs bottle's
      hooks/pre-receive here, in the host repo, before any ref moves. The
      repo's own hooks never run for these pushes.

Nothing is mirrored or watched: every request sees the repo as it is right now.
"""

import asyncio
import hashlib
import logging
import os
import subprocess
import time
import zlib
from dataclasses import dataclass
from pathlib import Path

log = logging.getLogger("bottle.egress")

HOST = "bottle.host"
GIT_PATH = "/git"
ORIGIN, HOST_REMOTE = "origin", "host"
MAX_BODY = 64 * 1024 * 1024
HOOKS = Path(__file__).resolve().parent / "hooks"
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

    async def handle(self, request: HttpRequest, writer: asyncio.StreamWriter) -> str:
        """Answer a request to bottle.host; returns an outcome for the log."""
        remote, _, path_info = request.path.removeprefix(GIT_PATH + "/").partition("/")
        if self.git_dir is not None and request.path.startswith(GIT_PATH + "/") and remote in (ORIGIN, HOST_REMOTE):
            return await self._git(remote, "/" + path_info, request, writer)
        await _respond(writer, 404, "Not Found", b"no such bottle.host service\n")
        return "not-found"

    async def _git(self, remote: str, path_info: str, request: HttpRequest, writer: asyncio.StreamWriter) -> str:
        push = "git-receive-pack" in path_info or "service=git-receive-pack" in request.query
        if push and remote == ORIGIN:
            await _respond(writer, 403, "Forbidden", b"origin is read-only; push agent/* branches to host\n")
            return "denied"
        config: dict[str, str] = {}
        if remote == ORIGIN:
            if not push and "info/refs" in path_info:  # once per fetch, at its start
                await pull_through(self.git_dir)
            root = await asyncio.to_thread(write_view, self.git_dir, self.view_dir or default_view_dir(self.git_dir))
        else:
            root = self.git_dir
            if push:
                config = {
                    "http.receivepack": "true",
                    "core.hooksPath": str(HOOKS),  # bottle's rules; the repo's own hooks never run
                    "receive.denyDeletes": "true",
                    "receive.denyNonFastForwards": "true",
                    "receive.fsckObjects": "true",
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


async def _relay_cgi(output: bytes, writer: asyncio.StreamWriter) -> None:
    """Turn a CGI response (headers, blank line, body) into an HTTP one."""
    head, _, body = output.partition(b"\r\n\r\n")
    if not _:
        head, _, body = output.partition(b"\n\n")
    status = b"200 OK"
    lines = []
    for line in head.replace(b"\r\n", b"\n").split(b"\n"):
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"status":
            status = value.strip()
        elif line:
            lines.append(line)
    writer.write(b"HTTP/1.1 " + status + b"\r\n" + b"".join(l + b"\r\n" for l in lines))
    writer.write(f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body)
    await writer.drain()


async def _respond(writer: asyncio.StreamWriter, status: int, reason: str, body: bytes) -> None:
    writer.write(
        f"HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {len(body)}\r\n"
        f"Connection: close\r\n\r\n".encode() + body
    )
    await writer.drain()
