"""The sandbox engine: everything folio asks of bottles, in this process (see bottle/bottles.py).

What it needs is small -- validate a spec, create a bottle, read its state,
start and delete it, run commands in it with pipes or a terminal. A command
runs as bottle-cli's exec, on the machine the bottle is; the rest is
bottles.py's. A bottle's failures are FolioErrors, saying what bottle said.

$FOLIO_ENGINE, if it's set, is a file defining these functions instead, which
stand in for bottle (folio's tests' does: tests/cli_engine.py).
"""

import importlib.util
import os
import subprocess
import sys
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

from .errors import FolioError

# bottle's package, beside folio's in this checkout: shared, until what folio uses of it is folio's own.
_ROOT = str(Path(__file__).resolve().parents[2])
if _ROOT not in sys.path:
    sys.path.insert(0, _ROOT)

from bottle import bottles, runners, specs  # noqa: E402
from bottle.errors import BottleError  # noqa: E402


@contextmanager
def _said() -> Iterator[None]:
    """A bottle's failure, as folio's."""
    try:
        yield
    except BottleError as e:
        raise FolioError(str(e)) from None


def validate_spec(spec: dict) -> dict:
    """The spec in canonical form, or bottle's reason it isn't one."""
    with _said():
        return specs.canonical(spec)


@dataclass(frozen=True)
class Mount:
    """A repo for a bottle: checked out at `path`, from `checkout` (None: bottle's default), served `remotes`."""

    path: str
    repo: Path
    checkout: str | None
    remotes: list[str]


def new(owner: Path, name: str, spec: dict, mounts: list[Mount], context: str, runner: str | None = None,
        ssh: dict[str, str] | None = None) -> None:
    """Create a bottle for the thread whose directory is `owner`, where its record is kept, on `runner` (None:
    this machine), that may ssh to each bottle `ssh` gives, by the name given: {name: bottle}. On this terminal: an image build
    shows, and a login can ask."""
    print(f"Creating bottle {name}...", file=sys.stderr)
    requests = [bottles.MountRequest(m.path, m.repo, m.checkout, m.remotes or None) for m in mounts]
    with _said():
        bottles.create(owner, spec, name, requests, context, runner=runner or runners.LOCAL, ssh=ssh)


def status(names: list[str]) -> dict[str, dict]:
    """Each named bottle's status row (see bottles.status); state "missing" if there's no such bottle."""
    if not names:
        return {}
    with _said():
        return {row["name"]: row for row in bottles.status(names)}


def start(name: str) -> None:
    """Start a stopped bottle. On this terminal, as `new` is: starting can need a login."""
    with _said():
        bottles.start(name)


def delete(name: str) -> None:
    with _said():
        bottles.delete(name)


def touch(name: str) -> None:
    """Record that the bottle is being used now."""
    with _said():
        bottles.touch(name)


def _command(name: str, argv: list[str], tty: bool = False) -> list[str]:
    with _said():
        return bottles.exec_command(name, argv, tty)


def failed(name: str, argv: list[str], returncode: int) -> FolioError:
    """That a command in a bottle failed; what it said, it said on stderr."""
    return FolioError(f"`{argv[0]}` in {name} failed (exit {returncode})")


def run(name: str, argv: list[str], input: str | None = None, check: bool = True) -> subprocess.CompletedProcess:
    """Run a command in the bottle, with pipes. Without `check`, a failure is the caller's to expect, so its stderr
    is dropped rather than shown as if something went wrong."""
    # Never this terminal for stdin: an exec passes it through to the bottle, which would read what's typed at
    # folio's own prompts.
    result = subprocess.run(_command(name, argv), stdout=subprocess.PIPE, text=True,
                            stderr=None if check else subprocess.DEVNULL,
                            input=input, stdin=None if input is not None else subprocess.DEVNULL)
    if check and result.returncode != 0:
        raise failed(name, argv, result.returncode)
    return result


def run_to_file(name: str, argv: list[str], path: Path) -> None:
    """Run a command in the bottle with its stdout, bytes and all, going to `path`."""
    with path.open("wb") as out:
        result = subprocess.run(_command(name, argv), stdin=subprocess.DEVNULL, stdout=out)
    if result.returncode != 0:
        raise failed(name, argv, result.returncode)


def run_from_file(name: str, argv: list[str], path: Path) -> None:
    """Run a command in the bottle with `path` as its stdin."""
    with path.open("rb") as in_:
        result = subprocess.run(_command(name, argv), stdin=in_, stdout=subprocess.DEVNULL)
    if result.returncode != 0:
        raise failed(name, argv, result.returncode)


def attach(name: str, argv: list[str]) -> NoReturn:
    """Replace folio with a command in the bottle, on this terminal."""
    command = _command(name, argv, tty=sys.stdin.isatty() and sys.stdout.isatty())
    os.execvp(command[0], command)


STANDS_IN = ("validate_spec", "new", "status", "start", "delete", "touch", "run", "run_to_file", "run_from_file",
             "attach")

if os.environ.get("FOLIO_ENGINE"):
    _spec = importlib.util.spec_from_file_location("folio_engine_stand_in", os.environ["FOLIO_ENGINE"])
    _stand_in = importlib.util.module_from_spec(_spec)
    _spec.loader.exec_module(_stand_in)
    _functions: dict[str, Callable] = {name: getattr(_stand_in, name) for name in STANDS_IN}
    globals().update(_functions)
