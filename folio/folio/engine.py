"""The sandbox engine: everything folio asks of bottle, through bottle's CLI.

Folio never imports bottle. What it needs is small -- validate a spec, create
a bottle, read its state, start and delete it, run commands in it with pipes
or a terminal -- so another engine could stand in, if it can also serve git
remotes the way bottle does (`bottle new --git-remote`).

Bottle's errors are bottle's: its stderr reaches the user as bottle wrote
it, never captured, reworded or parsed, and folio adds only which bottle
command failed. What folio reads is bottle's stdout, from commands made to be
read (`--json`).

Which bottle: $FOLIO_BOTTLE if set, else the `bin/bottle` beside folio in this
checkout, else `bottle` on PATH.
"""

import json
import os
import subprocess
import tempfile
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import NoReturn

from .errors import FolioError

SIBLING = Path(__file__).resolve().parents[2] / "bin" / "bottle"


def command() -> list[str]:
    if os.environ.get("FOLIO_BOTTLE"):
        return [os.environ["FOLIO_BOTTLE"]]
    return [str(SIBLING)] if SIBLING.exists() else ["bottle"]


def _run(*args: str, input: str | None = None, check: bool = True) -> subprocess.CompletedProcess:
    """Run bottle, reading its stdout. Without `check`, a failure is the caller's to expect,
    so its stderr is dropped rather than shown as if something went wrong."""
    # Never this terminal for stdin: `bottle exec` passes it through to the
    # bottle, which would read what's typed at folio's own prompts.
    result = subprocess.run([*command(), *args], stdout=subprocess.PIPE, text=True,
                            stderr=None if check else subprocess.DEVNULL,
                            input=input, stdin=None if input is not None else subprocess.DEVNULL)
    if check and result.returncode != 0:
        raise failed(args, result.returncode)
    return result


def failed(args: tuple[str, ...] | list[str], returncode: int) -> FolioError:
    """That a bottle command failed; bottle has already said why, on stderr."""
    words = []
    for arg in args:
        if arg.startswith("-") or "/" in arg:
            break
        words.append(arg)
    return FolioError(f"`bottle {' '.join(words)}` failed (exit {returncode})")


def validate_spec(spec: dict) -> dict:
    """The spec in canonical form, or bottle's reason it isn't one."""
    with _file(json.dumps(spec)) as path:
        return json.loads(_run("spec", "validate", "--json", path).stdout)


def new(name: str, spec: dict, git_repo: Path, remotes: list[str], checkout: str | None, context: str) -> None:
    """Create a bottle. Attached to this terminal: an image build shows, and a login can ask."""
    with _file(json.dumps(spec)) as spec_path, _file(context) as context_path:
        argv = [*command(), "new", "--spec", spec_path, "--name", name, "--git-repo", str(git_repo),
                *[arg for remote in remotes for arg in ("--git-remote", remote)],
                *(["--checkout", checkout] if checkout else []), "--context", context_path]
        result = subprocess.run(argv)
        if result.returncode != 0:
            raise failed(["new", name], result.returncode)


def status(names: list[str]) -> dict[str, dict]:
    """Each named bottle's status row (see `bottle status --json`); state "missing" if there's no such bottle."""
    if not names:
        return {}
    return {row["name"]: row for row in json.loads(_run("status", "--json", *names).stdout)}


def start(name: str) -> None:
    """Start a stopped bottle. Attached to this terminal, as `new` is: starting can need a login."""
    result = subprocess.run([*command(), "start", name], stdout=subprocess.DEVNULL)
    if result.returncode != 0:
        raise failed(["start", name], result.returncode)


def delete(name: str) -> None:
    _run("delete", name)


def run(name: str, argv: list[str], input: str | None = None, check: bool = True) -> subprocess.CompletedProcess:
    """Run a command in the bottle, with pipes."""
    return _run("exec", name, "--", *argv, input=input, check=check)


def run_to_file(name: str, argv: list[str], path: Path) -> None:
    """Run a command in the bottle with its stdout, bytes and all, going to `path`."""
    with path.open("wb") as out:
        result = subprocess.run([*command(), "exec", name, "--", *argv], stdin=subprocess.DEVNULL, stdout=out)
    if result.returncode != 0:
        raise failed(["exec", name], result.returncode)


def run_from_file(name: str, argv: list[str], path: Path) -> None:
    """Run a command in the bottle with `path` as its stdin."""
    with path.open("rb") as in_:
        result = subprocess.run([*command(), "exec", name, "--", *argv], stdin=in_, stdout=subprocess.DEVNULL)
    if result.returncode != 0:
        raise failed(["exec", name], result.returncode)


def attach(name: str, argv: list[str]) -> NoReturn:
    """Replace folio with a command in the bottle, on this terminal."""
    cmd = [*command(), "exec", name, "--", *argv]
    os.execvp(cmd[0], cmd)


@contextmanager
def _file(content: str) -> Iterator[str]:
    """A temporary file holding `content`, for as long as bottle needs to read it."""
    fd, path = tempfile.mkstemp(prefix="folio-")
    try:
        with os.fdopen(fd, "w") as f:
            f.write(content)
        yield path
    finally:
        os.unlink(path)
