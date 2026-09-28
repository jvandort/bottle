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


def image_labels(image: str) -> dict[str, str] | None:
    """The image's labels, or None if there's no such image."""
    result = subprocess.run(["container", "image", "inspect", image], capture_output=True, text=True)
    if result.returncode != 0:
        return None
    [info] = json.loads(result.stdout)
    labels: dict[str, str] = {}
    for variant in info.get("variants", []):
        labels.update(variant.get("config", {}).get("config", {}).get("Labels") or {})
    return labels


@dataclass(frozen=True)
class ImageRef:
    name: str  # e.g. bottle/base:latest
    digest: str


def images() -> list[ImageRef]:
    """Every tagged image."""
    refs = []
    for image in json.loads(_run("image", "list", "--format", "json")):
        descriptor = image["configuration"]["descriptor"]
        name = descriptor.get("annotations", {}).get("com.apple.containerization.image.name")
        if name:
            refs.append(ImageRef(name, descriptor["digest"]))
    return refs


def images_in_use() -> set[str]:
    """Digests of the images every container, running or not, was created from."""
    return {c["configuration"]["image"]["descriptor"]["digest"] for c in json.loads(_run("list", "--all", "--format", "json"))}


def image_delete(image: str) -> None:
    _run("image", "delete", image)


def network_exists(network: str) -> bool:
    return _succeeds("network", "inspect", network)


def network_create(network: str, labels: dict[str, str] | None = None) -> None:
    """Create a host-only network: it reaches the host and nothing else."""
    _run("network", "create", "--internal", *_label_options(labels), network)


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
    name: str, image: str, network: str, env: dict[str, str], mounts: list[Mount], labels: dict[str, str] | None = None
) -> None:
    """Create and start a detached container running the image's default command."""
    cmd = ["run", "--detach", "--name", name, "--network", network, *_label_options(labels)]
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


@dataclass(frozen=True)
class ContainerInfo:
    state: str
    labels: dict[str, str]
    networks: list[str]


def container_info(name: str) -> ContainerInfo | None:
    result = subprocess.run(["container", "inspect", name], capture_output=True, text=True)
    if result.returncode != 0:
        return None
    [info] = json.loads(result.stdout)
    config = info["configuration"]
    return ContainerInfo(
        info["status"]["state"], config.get("labels") or {}, [n["network"] for n in config.get("networks") or []]
    )


def container_delete(name: str, owner: tuple[str, str] | None = None) -> None:
    """Stop and delete `name`; a no-op if it doesn't exist.

    With `owner` (label value, network), delete only a container that's
    provably that owner's: labelled with it, or (made before labels) attached
    to its network. Anything else is left alone and reported.
    """
    info = container_info(name)
    if info is None:
        return
    if owner is not None and not _owned(info, *owner):
        raise BottleError(f"container {name} isn't this bottle's; leaving it alone")
    _run("delete", "--force", name)


OWNER_LABEL = "bottle.id"


def _owned(info: ContainerInfo, owner: str, network: str) -> bool:
    if OWNER_LABEL in info.labels:
        return info.labels[OWNER_LABEL] == owner
    return network in info.networks


def _label_options(labels: dict[str, str] | None) -> list[str]:
    return [option for key, value in (labels or {}).items() for option in ("--label", f"{key}={value}")]


def container_exec(
    name: str, argv: list[str], user: str | None = None, workdir: str | None = None, input: str | None = None
) -> str:
    """Run a command in a running container and return its output. `input` is sent to its stdin."""
    if input is None:
        return _run("exec", *_exec_options(user, workdir), name, *argv)
    return _run("exec", "--interactive", *_exec_options(user, workdir), name, *argv, input=input)


def exec_command(name: str, argv: list[str], user: str | None = None, tty: bool = False) -> list[str]:
    """The command line that runs `argv` in a container with stdin attached (and a TTY), for other tools to run."""
    return ["container", "exec", "--interactive", *(["--tty"] if tty else []), *_exec_options(user, None), name, *argv]


def container_exec_interactive(
    name: str, argv: list[str], user: str | None = None, workdir: str | None = None, env: dict[str, str] | None = None
) -> NoReturn:
    """Replace this process with an interactive command in the container, attached to the terminal.

    `env` is passed by name only (`--env NAME`, the value taken from this
    process's environment), so values never appear on a command line.
    """
    names = []
    for key, value in (env or {}).items():
        os.environ[key] = value
        names += ["--env", key]
    os.execvp("container", ["container", "exec", "--interactive", "--tty", *names, *_exec_options(user, workdir), name, *argv])


def _exec_options(user: str | None, workdir: str | None) -> list[str]:
    options = []
    if user:
        options += ["--user", user]
    if workdir:
        options += ["--workdir", workdir]
    return options


async def build(
    context: Path, tag: str, build_args: dict[str, str], no_cache: bool = False, dockerfile: Path | None = None,
    labels: dict[str, str] | None = None,
) -> None:
    """Build the image at `context` (from `dockerfile`, default its Dockerfile), streaming progress."""
    cmd = ["container", "build", "--tag", tag]
    for key, value in (labels or {}).items():
        cmd += ["--label", f"{key}={value}"]
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


def _run(*args: str, input: str | None = None) -> str:
    result = subprocess.run(["container", *args], capture_output=True, text=True, input=input)
    if result.returncode != 0:
        raise BottleError(result.stderr.strip() or f"container {' '.join(args)} failed")
    return result.stdout
