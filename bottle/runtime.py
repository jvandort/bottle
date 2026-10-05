"""The container runtime (apple/container). All `container` invocations live here."""

import asyncio
import errno
import json
import os
import socket
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

from bottle.errors import BottleError


def services_running() -> bool:
    """Whether container's system services are up. Nothing else here works without them."""
    result = subprocess.run(["container", "system", "status"], capture_output=True, text=True)
    return result.returncode == 0 and any(
        line.split() == ["status", "running"] for line in result.stdout.splitlines()
    )


def start_services() -> None:
    """Start container's system services, with their output visible. Installs nothing, so needs no prompt."""
    print("Starting container services...", file=sys.stderr)
    if subprocess.run(["container", "system", "start", "--disable-kernel-install"]).returncode != 0:
        raise BottleError("`container system start` failed")


def builder_start() -> None:
    """Start the image builder VM, creating it if needed. A no-op when it's running."""
    _run("builder", "start")


def builder_running() -> bool:
    result = subprocess.run(["container", "builder", "status", "--format", "json"], capture_output=True, text=True)
    if result.returncode != 0:
        return False
    try:
        return any(b.get("status", {}).get("state") == "running" for b in json.loads(result.stdout or "[]"))
    except (json.JSONDecodeError, AttributeError):
        return False


def builder_stop() -> None:
    _run("builder", "stop")


def network_gateway(network: str = "default") -> str:
    """The host's address on `network`, where bottle serves the network's egress proxy."""
    [info] = json.loads(_run("network", "inspect", network))
    return info["status"]["ipv4Gateway"]


def host_has_address(address: str) -> bool:
    """Whether `address` is one of this machine's own, i.e. a network's bridge is up with its gateway.

    apple/container can lose a network's bridge while its containers keep
    running and the network still reports the gateway; they're then cut off.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        try:
            s.bind((address, 0))
        except OSError as e:
            if e.errno == errno.EADDRNOTAVAIL:
                return False
            raise
    return True


def image_exists(image: str) -> bool:
    return _inspect("image", "inspect", image) is not None


def image_labels(image: str) -> dict[str, str] | None:
    """The image's labels, or None if there's no such image."""
    info = _inspect("image", "inspect", image)
    if info is None:
        return None
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
    return _inspect("network", "inspect", network) is not None


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
    """Create and start a detached container running the image's default command.

    It gets the whole machine: every CPU core and all of its memory. The VM only
    takes memory from the host as the guest uses it, so this costs nothing up
    front, but memory the guest has used isn't given back until it stops.
    """
    cpus, memory = host_resources()
    cmd = ["run", "--detach", "--name", name, "--network", network, "--cpus", str(cpus), "--memory", memory]
    cmd += _label_options(labels)
    for key, value in env.items():
        cmd += ["--env", f"{key}={value}"]
    for m in mounts:
        cmd += ["--mount", f"type=bind,source={m.source},target={m.target}" + (",readonly" if m.readonly else "")]
    _run(*cmd, image)


def host_resources() -> tuple[int, str]:
    """The machine's CPU cores and memory, as `container run --cpus` and `--memory` values."""
    memsize = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, check=True).stdout)
    return os.cpu_count() or 1, f"{memsize // (1024 * 1024)}M"


def container_state(name: str) -> str | None:
    """"running", "stopped", etc., or None if there's no such container."""
    info = _inspect("inspect", name)
    return None if info is None else info["status"]["state"]


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
    info = _inspect("inspect", name)
    if info is None:
        return None
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
    name: str, argv: list[str], user: str | None = None, workdir: str | None = None, tty: bool = True,
) -> NoReturn:
    """Replace this process with a command in the container, attached to this terminal.

    Nothing is passed in from the host: the bottle's environment is the image's
    (a feature's containerEnv), and credentials never enter a bottle at all
    (see auth.py). Without `tty` the command's stdout is a pipe, which is what
    a caller redirecting or piping the output wants.
    """
    options = ["--interactive", *(["--tty"] if tty else [])]
    os.execvp("container", ["container", "exec", *options, *_exec_options(user, workdir), name, *argv])


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


def _inspect(*args: str) -> dict | None:
    """What `container ARGS` (an inspect) reports about one object, or None if there's no such object.

    A failed inspect means the object is absent only if the services that would
    have found it are up. When they're down (as after the machine restarts)
    they're started and the inspect retried; read as absence instead, every
    container would look gone, and a delete would forget a bottle whose
    container and network it never removed.
    """
    result = subprocess.run(["container", *args], capture_output=True, text=True)
    if result.returncode != 0 and not services_running():
        start_services()
        result = subprocess.run(["container", *args], capture_output=True, text=True)
    if result.returncode != 0:
        return None
    found = json.loads(result.stdout or "[]")
    return found[0] if found else None


def _run(*args: str, input: str | None = None) -> str:
    result = subprocess.run(["container", *args], capture_output=True, text=True, input=input)
    if result.returncode != 0:
        raise BottleError(result.stderr.strip() or f"container {' '.join(args)} failed")
    return result.stdout
