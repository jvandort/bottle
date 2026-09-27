"""Bottle images, built from containers/images/<name>/ in this repo.

Builds reach the network only through a temporary egress proxy on the host.
The builder VM's own DNS goes through container's gateway forwarder, which
stops answering when any other host process holds port 53 (VPN clients and
local resolvers commonly do): https://github.com/apple/container/issues/402.
Going through the host also gives builds the host's routes, e.g. a VPN.
"""

import asyncio
import re
import sys
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


def tag(name: str) -> str:
    return f"bottle/{name}:latest"


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


def ensure_built(name: str) -> None:
    """Build `name`, and whatever it's built from, if not built yet."""
    for image in build_order(name):
        if not runtime.image_exists(tag(image)):
            print(f"Building {tag(image)}...", file=sys.stderr)
            _build_one(image)


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
    build_context(IMAGES / name, tag(name), no_cache)


def build_context(context: Path, image: str, no_cache: bool = False, dockerfile: Path | None = None) -> None:
    """Build `context` into `image`, reaching the network only through a temporary egress proxy."""
    prereqs.ensure_container()
    # The builder VM must be running before its network's gateway exists on the host.
    runtime.builder_start()
    gateway = runtime.network_gateway()
    asyncio.run(_build_via_proxy(context, image, gateway, no_cache, dockerfile))


async def _build_via_proxy(context: Path, image: str, gateway: str, no_cache: bool, dockerfile: Path | None) -> None:
    server = await egress.EgressProxy(f"build {image}", egress.Policy()).start(gateway, 0)
    port = server.sockets[0].getsockname()[1]
    proxy = f"http://{gateway}:{port}"
    # BuildKit predefines these args: they reach RUN steps but aren't stored in the image,
    # and changing them (e.g. a new port per build) doesn't invalidate the cache.
    build_args = {key: proxy for key in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY")}
    async with server:
        await runtime.build(context, image, build_args, no_cache, dockerfile)
