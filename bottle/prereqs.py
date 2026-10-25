"""Host prerequisites, set up on first use.

Commands call the ensure_* function for what they need. Checks are silent
when everything is in place. Anything that installs software asks first.
"""

import functools
import platform
import shutil
import subprocess
import sys
from bottle import smolvm
from bottle.errors import BottleError


@functools.cache
def ensure_smolvm() -> None:
    """smolvm installed (see smolvm.py), or the one $BOTTLE_SMOLVM names."""
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise BottleError("bottle runs on macOS on Apple silicon")
    binary = smolvm.binary()
    if shutil.which(binary) is None:
        if binary != "smolvm":
            raise BottleError(f"BOTTLE_SMOLVM is {binary}, which isn't there")
        if shutil.which("brew") is None:
            raise BottleError("bottle installs smolvm with Homebrew; install it from https://brew.sh")
        _install("smolvm", ["brew", "install", "smol-machines/tap/smolvm"])


def _install(what: str, cmd: list[str]) -> None:
    command = " ".join(cmd)
    if not sys.stdin.isatty():
        raise BottleError(f"{what} is not installed; run `{command}`, or run bottle in a terminal to be prompted")
    answer = input(f"bottle needs {what}. Install it now with `{command}`? [Y/n] ")
    if answer.strip().lower() not in ("", "y", "yes"):
        raise BottleError(f"{what} is required")
    _run(cmd)


def _run(cmd: list[str]) -> None:
    """Run with output visible to the user, since installs can take a while."""
    if subprocess.run(cmd).returncode != 0:
        raise BottleError(f"`{' '.join(cmd)}` failed")
