"""Bottle images, built from containers/images/<name>/ in this repo.

Builds reach the network only through a temporary egress proxy on the host.
The builder VM's own DNS goes through container's gateway forwarder, which
stops answering when any other host process holds port 53 (VPN clients and
local resolvers commonly do): https://github.com/apple/container/issues/402.
Going through the host also gives builds the host's routes, e.g. a VPN.
"""

import asyncio
import hashlib
import re
import sys
from contextlib import contextmanager
from pathlib import Path

from bottle import egress, prereqs, runtime
from bottle.errors import BottleError

IMAGES = Path(__file__).resolve().parent.parent / "containers" / "images"
# The image every bottle starts from: the bottle contract. Not user-selectable;
# other images here would be alternative foundations meeting the same contract.
BASE = "base"
# A stage built on another bottle image, e.g. `FROM bottle/base:latest`.
FROM_BOTTLE = re.compile(r"^\s*FROM\s+(?:--\S+\s+)*bottle/([A-Za-z0-9._-]+)(?::latest)?(?:\s|$)", re.MULTILINE | re.IGNORECASE)


def available() -> list[str]:
    return sorted(p.parent.name for p in IMAGES.glob("*/Dockerfile"))


# Every image bottle builds records a hash of what it was built from. An image
# whose inputs have changed since is stale: it's rebuilt when next needed, and
# deleted once no container uses it.
INPUTS_LABEL = "bottle.inputs"


def tag(name: str) -> str:
    return f"bottle/{name}:latest"


def tree_hash(path: Path) -> str:
    """A hash of every file under `path`: names, contents and whether each is executable."""
    digest = hashlib.sha256()
    for file in sorted(p for p in path.rglob("*") if p.is_file()):
        digest.update(f"{file.relative_to(path)}\0{file.stat().st_mode & 0o111:o}\0".encode())
        digest.update(hashlib.sha256(file.read_bytes()).digest())
    return digest.hexdigest()


def inputs_hash(name: str) -> str:
    """What the image `name` is built from: its directory, and the images it's built on."""
    _require(name)
    digest = hashlib.sha256(tree_hash(IMAGES / name).encode())
    for dependency in dependencies(name):
        digest.update(inputs_hash(dependency).encode())
    return digest.hexdigest()


def is_current(image: str, expected: str) -> bool:
    labels = runtime.image_labels(image)
    return labels is not None and labels.get(INPUTS_LABEL) == expected


def dependencies(name: str) -> list[str]:
    """The bottle images `name` is built from, per its Dockerfile's FROM lines."""
    _require(name)
    return list(dict.fromkeys(FROM_BOTTLE.findall((IMAGES / name / "Dockerfile").read_text())))


def build_order(name: str) -> list[str]:
    """`name` and everything it's built from, dependencies first."""
    order: list[str] = []

    def visit(image: str, path: list[str]) -> None:
        if image in path:
            raise BottleError(f"images depend on each other in a cycle: {' -> '.join([*path, image])}")
        if image in order:
            return
        if image not in available():
            if not path:
                _require(image)
            raise BottleError(f"{path[-1]} is built from bottle/{image}, but there's no containers/images/{image}")
        for dependency in dependencies(image):
            visit(dependency, [*path, image])
        order.append(image)

    visit(name, [])
    return order


def ensure_built(name: str) -> bool:
    """Build `name`, and whatever it's built from, if missing or stale. True if anything was built."""
    built = False
    for image in build_order(name):
        if not is_current(tag(image), inputs_hash(image)):
            print(f"Building {tag(image)}...", file=sys.stderr)
            _build_one(image)
            built = True
    return built


def build(name: str, no_cache: bool = False) -> str:
    """Build the named image, first building anything it's built from that isn't built yet."""
    for dependency in build_order(name)[:-1]:
        ensure_built(dependency)
    _build_one(name, no_cache)
    return tag(name)


def _require(name: str) -> None:
    if name not in available():
        raise BottleError(f"no image named {name!r}; available: {', '.join(available())}")


def _build_one(name: str, no_cache: bool = False) -> None:
    _require(name)
    build_context(IMAGES / name, tag(name), inputs_hash(name), no_cache)


def build_context(
    context: Path, image: str, inputs: str, no_cache: bool = False, dockerfile: Path | None = None
) -> None:
    """Build `context` into `image`, reaching the network only through a temporary egress proxy.

    `inputs` is the hash of what the image is built from, recorded as a label.
    """
    prereqs.ensure_container()
    with building():
        # The builder VM must be running before its network's gateway exists on the host.
        _start_builder()
        gateway = runtime.network_gateway()
        asyncio.run(_build_via_proxy(context, image, gateway, no_cache, dockerfile, {INPUTS_LABEL: inputs}))


# The builder VM holds its memory (about 2 GB) for as long as it runs, so bottle
# stops it when it's done building, if bottle started it. A builder that was
# already running (someone else's build) is left running.
_building_depth = 0
_started_builder = False


@contextmanager
def building():
    """Keep the builder running for every build inside this block; stop it at the end if bottle started it.

    Nests, so building an image and the images underneath it starts the builder once.
    """
    global _building_depth, _started_builder
    _building_depth += 1
    try:
        yield
    finally:
        _building_depth -= 1
        if _building_depth == 0 and _started_builder:
            _started_builder = False
            try:
                runtime.builder_stop()
            except BottleError as e:
                print(f"bottle: couldn't stop the image builder: {e}", file=sys.stderr)


def _start_builder() -> None:
    global _started_builder
    if not _started_builder and not runtime.builder_running():
        _started_builder = True
    runtime.builder_start()


async def _build_via_proxy(
    context: Path, image: str, gateway: str, no_cache: bool, dockerfile: Path | None, labels: dict[str, str]
) -> None:
    server = await egress.EgressProxy(f"build {image}", egress.Policy()).start(gateway, 0)
    port = server.sockets[0].getsockname()[1]
    proxy = f"http://{gateway}:{port}"
    # BuildKit predefines these args: they reach RUN steps but aren't stored in the image,
    # and changing them (e.g. a new port per build) doesn't invalidate the cache.
    build_args = {key: proxy for key in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY")}
    async with server:
        await runtime.build(context, image, build_args, no_cache, dockerfile, labels)
