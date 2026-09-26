"""Host prerequisites, set up on first use.

Commands call the ensure_* function for what they need. Checks are silent
when everything is in place. Anything that installs software asks first.
"""

import functools
import platform
import shutil
import subprocess
import sys
from pathlib import Path

from bottle.errors import BottleError

MIN_MACOS = 26
# Where `container system kernel set --recommended` installs the default kernel.
KERNEL = Path.home() / "Library/Application Support/com.apple.container/kernels/default.kernel-arm64"


@functools.cache
def ensure_container() -> None:
    """apple/container installed, its services running, and a kernel installed."""
    _require_host()
    if shutil.which("container") is None:
        if shutil.which("brew") is None:
            raise BottleError("bottle installs apple/container with Homebrew; install it from https://brew.sh")
        _install("apple/container", ["brew", "install", "container"])
    if not _services_running():
        # Starting services installs nothing, so no prompt; the kernel is handled below.
        print("Starting container services...", file=sys.stderr)
        _run(["container", "system", "start", "--disable-kernel-install"])
    if not KERNEL.exists():
        _install("the recommended Linux kernel for containers", ["container", "system", "kernel", "set", "--recommended"])


def _require_host() -> None:
    if platform.system() != "Darwin":
        raise BottleError("bottle only runs on macOS")
    if platform.machine() != "arm64":
        raise BottleError("apple/container requires Apple silicon")
    version = platform.mac_ver()[0]
    if int(version.split(".")[0]) < MIN_MACOS:
        raise BottleError(f"macOS {MIN_MACOS}+ required (found {version})")


def _services_running() -> bool:
    result = subprocess.run(["container", "system", "status"], capture_output=True, text=True)
    return result.returncode == 0 and any(
        line.split() == ["status", "running"] for line in result.stdout.splitlines()
    )


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
