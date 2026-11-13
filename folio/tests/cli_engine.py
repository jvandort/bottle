"""folio's engine, standing in for bottles.py by running a bottle CLI: folio's tests' fake_bottle.py, which
$FOLIO_BOTTLE names ($FOLIO_ENGINE names this file; see folio/engine.py). What folio's engine was before it
called bottles.py in-process, kept for that.

Its errors are the CLI's: its stderr reaches the user as it wrote it, and folio adds only which command failed.
"""

import json
import os
import subprocess
import tempfile
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

from folio.errors import FolioError

def command() -> list[str]:
    return [os.environ["FOLIO_BOTTLE"]]


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


@dataclass(frozen=True)
class Mount:
    """A repo for a bottle: checked out at `path`, from `checkout` (None: bottle's default), served `remotes`."""

    path: str
    repo: Path
    checkout: str | None
    remotes: list[str]


def new(owner: Path, name: str, spec: dict, mounts: list[Mount], context: str, runner: str | None = None,
        ssh: dict[str, str] | None = None) -> None:
    """Create a bottle, on `runner` (None: bottle's default, this machine), that may ssh to each bottle `ssh`
    gives, by the name given: {name: bottle}. Attached to this terminal: an image build shows, and a login can
    ask."""
    with _file(json.dumps(spec)) as spec_path, _file(context) as context_path:
        argv = [*command(), "new", "--spec", spec_path, "--name", name, *(["--runner", runner] if runner else [])]
        for m in mounts:
            argv += ["--mount", f"{m.path}={m.repo}", *(["--checkout", f"{m.path}={m.checkout}"] if m.checkout else []),
                     *[arg for remote in m.remotes for arg in ("--git-remote", f"{m.path}:{remote}")]]
        argv += ["--context", context_path]
        argv += [arg for host, bottle in (ssh or {}).items() for arg in ("--ssh", f"{host}={bottle}")]
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


def touch(name: str) -> None:
    _run("touch", name)


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


def attach(name: str, argv: list[str], tty: bool | None = None) -> NoReturn:
    """Replace folio with a command in the bottle, on this terminal."""
    cmd = [*command(), "exec", *(["-t"] if tty else []), name, "--", *argv]
    os.execvp(cmd[0], cmd)


def copy(src: tuple[str | None, str], dst: tuple[str | None, str]) -> str:
    """Copy as `cp -r` would, between here and bottles: a bottle's path is in its mounts' clones, a relative one
    from its first mount (as exec runs there). Where it went, as the bottle has it if it's in one."""
    import shutil

    bottles_ = json.loads(Path(os.environ["FAKE_BOTTLE_STATE"]).read_text())["bottles"]

    def mounts(name: str) -> list[dict]:
        return sorted(bottles_[name]["mounts"], key=lambda m: len(m["path"]), reverse=True)

    def here(name: str | None, path: str) -> Path:
        if name is None:
            return Path(path)
        path = path if path.startswith("/") else f"{bottles_[name]['mounts'][0]['path']}/{path}"
        for mount in mounts(name):
            if path.startswith(mount["path"]):
                return Path(mount["dir"] + path[len(mount["path"]):])
        raise FolioError(f"{path} isn't in {name}'s mounts")

    from_, to = here(*src), here(*dst)
    target = to / from_.name if to.is_dir() else to
    if from_.is_dir():
        shutil.copytree(from_, target)
    else:
        shutil.copy2(from_, target)
    if dst[0] is None:
        return str(target)
    for mount in mounts(dst[0]):
        if str(target).startswith(mount["dir"]):
            return mount["path"] + str(target)[len(mount["dir"]):]
    return str(target)


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
