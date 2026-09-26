"""The container runtime (apple/container). All `container` invocations live here."""

import asyncio
import json
import subprocess
from pathlib import Path

from bottle.errors import BottleError


def builder_start() -> None:
    """Start the image builder VM, creating it if needed. A no-op when it's running."""
    _run("builder", "start")


def network_gateway(network: str = "default") -> str:
    """The host's address on `network`, where bottle serves the network's egress proxy."""
    [info] = json.loads(_run("network", "inspect", network))
    return info["status"]["ipv4Gateway"]


async def build(context: Path, tag: str, build_args: dict[str, str], no_cache: bool = False) -> None:
    """Build the image at `context`, streaming progress to the terminal."""
    cmd = ["container", "build", "--tag", tag]
    # Never --quiet: it hangs indefinitely in container 1.4.
    cmd += ["--progress", "auto"]
    if no_cache:
        cmd.append("--no-cache")
    for key, value in build_args.items():
        cmd += ["--build-arg", f"{key}={value}"]
    cmd.append(str(context))
    process = await asyncio.create_subprocess_exec(*cmd)
    if await process.wait() != 0:
        raise BottleError(f"building {tag} failed")


def _run(*args: str) -> str:
    result = subprocess.run(["container", *args], capture_output=True, text=True)
    if result.returncode != 0:
        raise BottleError(result.stderr.strip() or f"container {' '.join(args)} failed")
    return result.stdout
