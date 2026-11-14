"""folio serve: the process that answers what bottles' machines route to it, folio-cli's (`folio-cli serve`).

This is how folio talks to it, on its control socket in folio's home: a JSON line per request. bind() has it answer
a bottle's machine (starting it, if it isn't running), release_egress() stops that, and start() and stop() are
`folio serve start` and `stop`. bottle-cli decides a bottle's connections itself, by the policy its machine was made
with (services.network), and gives folio serve the ones that policy routes to it, on a socket per service
(services_sockets); folio-cli's serve/ says what it does with them.

A bind says all folio serve needs of a bottle (its machine, runner, repos, features, permissions, labels and ssh
grants, and what to forward to its runner), so folio serve reads no record of folio's; when it starts, it's given
the bottles whose machines are running (bottles.bind_messages), to answer before anyone else. A command finds it
running the folio-cli this folio runs, or restarts it first: one running another may not know what's asked of it.
"""

import hashlib
import json
import logging
import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

from folio.bottle.errors import BottleError
from folio.bottle.host import Served
from folio.bottle.store import folio_home, log_path, open_log

log = logging.getLogger("bottle.daemon")
log.addHandler(logging.NullHandler())

START_TIMEOUT = 5
# macOS caps Unix socket paths at 104 bytes.
MAX_SOCKET_PATH = 100
# Its log, in folio's home.
LOG = "serve"
# What it serves, each on a socket of its own, which a machine's routes name (services.network).
SERVICES = ("host", "ssh", "credentials")
# bottle's hooks, which a push from a bottle runs (folio-cli's serve/git.rs).
HOOKS = Path(__file__).resolve().parent / "hooks"


def home() -> Path:
    """folio's home: where folio serve keeps its sockets and its log."""
    return folio_home()


def socket_path(name: str = "serve.sock") -> Path:
    path = home() / name
    if len(str(path)) <= MAX_SOCKET_PATH:
        return path
    # A deep home: a short per-user directory instead, keyed by where it would have been.
    key = hashlib.sha256(str(path).encode()).hexdigest()[:16]
    return Path(f"/tmp/bottle-{os.getuid()}/{key}.sock")


def services_sockets() -> dict[str, Path]:
    """Where folio serve answers what machines route to it, by service: bottle.host's (git, messages), ssh to
    bottles, and credentials' hosts."""
    return {name: socket_path(f"services/{name}.sock") for name in SERVICES}


def code_hash() -> str:
    """The folio-cli this folio runs, by a hash of it: what folio serve says it's running when asked (ping)."""
    from folio.bottle import krun

    program = shutil.which(krun.folio_binary()) or krun.folio_binary()
    try:
        return hashlib.sha256(Path(program).read_bytes()).hexdigest()[:16]
    except OSError:
        return ""


def bind_message(bottle: str, machine: str, repos: tuple[Served, ...] = (), features: tuple[str, ...] | list[str] = (),
                 runner: str = "local", id: str = "", permissions: tuple[str, ...] | list[str] = (),
                 labels: dict[str, str] | None = None, repo: str | None = None, ssh: tuple[dict, ...] | list[dict] = (),
                 forwards: list[str] | None = None) -> dict:
    """What has folio serve answer what `machine`, on `runner`, routes to it as `bottle`'s: its repos, features
    (whose credentials it attaches), permissions, labels and first repo's name (as the list of bottles has them),
    the bottles it may ssh to ({name, bottle, id, address}), and, on a runner, what to forward there (else the
    runner's own, runners.Runner.forwards)."""
    if forwards is None and runner != "local":
        from folio.bottle import runners  # runners imports this module

        forwards = runners.get(runner).forwards()
    return {
        "op": "bind", "bottle": bottle, "machine": machine, "runner": runner, "id": id,
        "features": list(features), "permissions": list(permissions), "labels": dict(labels or {}), "repo": repo,
        "ssh": list(ssh), "forwards": list(forwards or ()),
        "repos": [{"git_dir": str(r.git_dir) if r.git_dir else None, "at": r.at,
                   "remotes": [remote.spec for remote in r.remotes]} for r in repos],
    }


def bind(bottle: str, machine: str, repos: tuple[Served, ...] = (), features: tuple[str, ...] | list[str] = (),
         runner: str = "local", **given) -> None:
    """Have folio serve answer what `machine`, on `runner`, routes to it as `bottle`'s (bind_message says what else
    it's told), starting folio serve if it isn't running. Its credentials are read as it's bound: a login reaches a
    bottle when it's next started, or when folio serve is."""
    _request(bind_message(bottle, machine, repos, features, runner, **given), start=True)


def release_egress(bottle: str) -> None:
    """Stop answering `bottle`'s machine, if folio serve is running at all."""
    _request({"op": "release", "bottle": bottle}, start=False)


def start() -> bool:
    """Start folio serve in the background. False if it was already running, the folio-cli this folio runs."""
    try:
        _send({"op": "ping"})
    except (FileNotFoundError, ConnectionRefusedError):
        _start_daemon()
        return True
    return _ensure_current()


def stop() -> bool:
    """Stop folio serve and every egress it serves. False if it wasn't running."""
    try:
        reply = _send({"op": "shutdown"})
        log.info("asked folio serve to stop")
    except (FileNotFoundError, ConnectionRefusedError):
        return False
    if not reply.get("ok"):
        raise BottleError(f"folio serve: {reply.get('error', 'shutdown failed')}")
    return True


# Whether this process has seen folio serve running this folio's folio-cli: asked once a command.
_current = False


def _ensure_current() -> bool:
    """folio serve running this folio's folio-cli, if it's running: one running another is restarted first.

    Running bottles keep their tokens, and the new folio serve binds them again as it starts, so they lose only
    the connections they open in between. Returns whether it restarted it.
    """
    global _current
    if _current:
        return False
    try:
        reply = _send({"op": "ping"})
    except (FileNotFoundError, ConnectionRefusedError):
        return False  # not running: whoever starts it starts this one
    except _Closed:
        _wait_gone()  # on its way out: whoever starts it next starts this one
        return False
    _current = True
    if reply.get("code") == code_hash():
        return False
    log.info("folio serve is running other code (%s); restarting it", reply.get("code", "from before it said"))
    print("bottle: folio serve was running other code; restarting it", file=sys.stderr)
    stop()
    _wait_gone()
    _start_daemon()
    return True


def _wait_gone() -> None:
    """Until the folio serve that's stopping is gone, or would the new one find it and leave. Until then it
    answers, or, winding up, takes a connection and closes it unanswered."""
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        try:
            _send({"op": "ping"})
        except (FileNotFoundError, ConnectionRefusedError):
            return
        except _Closed:
            pass
        time.sleep(0.05)


def _request(message: dict, start: bool, timeout: float = 30) -> dict:
    _ensure_current()
    try:
        try:
            reply = _send(message, timeout)
        except (FileNotFoundError, ConnectionRefusedError):
            if not start:
                return {"ok": True}
            _start_daemon()
            reply = _send(message, timeout)
    except _Closed:
        raise BottleError(f"folio serve closed the connection without answering {message['op']}: "
                          f"is it stopping? See {log_path(LOG, home())}") from None
    if not reply.get("ok"):
        raise BottleError(f"folio serve: {reply.get('error', 'request failed')}")
    return reply


class _Closed(ConnectionError):
    """folio serve took the connection and closed it unanswered: what one winding up does."""


def _send(message: dict, timeout: float = 30) -> dict:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        s.connect(str(socket_path()))
        try:
            s.sendall(json.dumps(message).encode() + b"\n")
            line = s.makefile().readline()
        except (BrokenPipeError, ConnectionResetError):
            raise _Closed from None
        if not line:
            raise _Closed
        return json.loads(line)


def _launch(background: bool) -> subprocess.Popen:
    """folio-cli serve, on folio serve's sockets, given the bottles to answer (bottles.bind_messages): in a session
    of its own and in /, for one that outlives this command (a machine's directory may be deleted while it runs,
    and then every git it runs fails), its output in its log; else here."""
    from folio.bottle import bottles, features, krun  # which import this module

    restore = json.dumps(bottles.bind_messages())
    command = [krun.folio_binary(), "serve", "--control", str(socket_path()), "--restore",
               *(arg for name, path in services_sockets().items() for arg in ("--service", f"{name}={path}"))]
    env = {**os.environ, "FOLIO_HOME": str(home()), "FOLIO_FEATURES": str(features.FEATURES),
           "FOLIO_HOOKS": str(HOOKS), "BOTTLE_PYTHON": sys.executable, "BOTTLE_CLI": krun.binary()}
    try:
        if background:
            with os.fdopen(open_log(LOG, home()), "a") as out:
                process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=out, stderr=out,
                                           start_new_session=True, cwd="/", env=env, text=True)
        else:
            process = subprocess.Popen(command, stdin=subprocess.PIPE, env=env, text=True)
    except FileNotFoundError:
        raise BottleError(krun.RunnerCli("").missing()) from None
    process.stdin.write(restore)
    process.stdin.close()
    return process


def _start_daemon() -> None:
    log.info("starting folio serve")
    _launch(background=True)
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        try:
            _send({"op": "ping"})
            return
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.05)
    raise BottleError(f"folio serve didn't start; see {log_path(LOG, home())}")


def run() -> int:
    """folio serve, in this process's place: `folio serve start --foreground`."""
    return _launch(background=False).wait()
