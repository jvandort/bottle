"""The container runtime (apple/container). All `container` invocations live here."""

import asyncio
import json
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

from bottle.errors import BottleError


def builder_start() -> None:
    """Start the image builder VM, creating it if needed. A no-op when it's running."""
    _run("builder", "start")


def network_gateway(network: str = "default") -> str:
    """The host's address on `network`, where bottle serves the network's egress proxy."""
    [info] = json.loads(_run("network", "inspect", network))
    return info["status"]["ipv4Gateway"]


def image_exists(image: str) -> bool:
    return _succeeds("image", "inspect", image)


def network_exists(network: str) -> bool:
    return _succeeds("network", "inspect", network)


def network_create(network: str) -> None:
    """Create a host-only network: it reaches the host and nothing else."""
    _run("network", "create", "--internal", network)


def network_delete(network: str) -> None:
    """Delete `network`; a no-op if it doesn't exist."""
    if network_exists(network):
        _run("network", "delete", network)


@dataclass(frozen=True)
class Mount:
    source: Path
    target: str
    readonly: bool = True


def container_run(
    name: str, image: str, network: str, env: dict[str, str], mounts: list[Mount]
) -> None:
    """Create and start a detached container running the image's default command."""
    cmd = ["run", "--detach", "--name", name, "--network", network]
    for key, value in env.items():
        cmd += ["--env", f"{key}={value}"]
    for m in mounts:
        cmd += ["--mount", f"type=bind,source={m.source},target={m.target}" + (",readonly" if m.readonly else "")]
    _run(*cmd, image)


def container_state(name: str) -> str | None:
    """"running", "stopped", etc., or None if there's no such container."""
    result = subprocess.run(["container", "inspect", name], capture_output=True, text=True)
    if result.returncode != 0:
        return None
    [info] = json.loads(result.stdout)
    return info["status"]["state"]


def container_start(name: str) -> None:
    _run("start", name)


def container_stop(name: str) -> None:
    _run("stop", name)


def container_delete(name: str) -> None:
    """Stop and delete `name`; a no-op if it doesn't exist."""
    if container_state(name) is not None:
        _run("delete", "--force", name)


def container_exec(name: str, argv: list[str], user: str | None = None, workdir: str | None = None) -> str:
    """Run a command in a running container and return its output."""
    return _run("exec", *_exec_options(user, workdir), name, *argv)


def exec_command(name: str, argv: list[str], user: str | None = None) -> list[str]:
    """The command line that runs `argv` in a container with stdin attached, for other tools to run."""
    return ["container", "exec", "--interactive", *_exec_options(user, None), name, *argv]


def container_exec_interactive(name: str, argv: list[str], user: str | None = None, workdir: str | None = None) -> NoReturn:
    """Replace this process with an interactive command in the container, attached to the terminal."""
    os.execvp("container", ["container", "exec", "--interactive", "--tty", *_exec_options(user, workdir), name, *argv])


def _exec_options(user: str | None, workdir: str | None) -> list[str]:
    options = []
    if user:
        options += ["--user", user]
    if workdir:
        options += ["--workdir", workdir]
    return options


async def build(
    context: Path, tag: str, build_args: dict[str, str], no_cache: bool = False, dockerfile: Path | None = None
) -> None:
    """Build the image at `context` (from `dockerfile`, default its Dockerfile), streaming progress."""
    cmd = ["container", "build", "--tag", tag]
    if dockerfile:
        cmd += ["--file", str(dockerfile)]
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


def _succeeds(*args: str) -> bool:
    return subprocess.run(["container", *args], capture_output=True).returncode == 0


def _run(*args: str) -> str:
    result = subprocess.run(["container", *args], capture_output=True, text=True)
    if result.returncode != 0:
        raise BottleError(result.stderr.strip() or f"container {' '.join(args)} failed")
    return result.stdout
