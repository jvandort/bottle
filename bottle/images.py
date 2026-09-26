"""Bottle images, built from containers/<name>/ in this repo.

Builds reach the network only through a temporary egress proxy on the host.
The builder VM's own DNS goes through container's gateway forwarder, which
stops answering when any other host process holds port 53 (VPN clients and
local resolvers commonly do): https://github.com/apple/container/issues/402.
Going through the host also gives builds the host's routes, e.g. a VPN.
"""

import asyncio
from pathlib import Path

from bottle import egress, prereqs, runtime
from bottle.errors import BottleError

CONTAINERS = Path(__file__).resolve().parent.parent / "containers"


def available() -> list[str]:
    return sorted(p.parent.name for p in CONTAINERS.glob("*/Dockerfile"))


def tag(name: str) -> str:
    return f"bottle/{name}:latest"


def build(name: str, no_cache: bool = False) -> str:
    """Build the named image and return its tag."""
    if name not in available():
        raise BottleError(f"no image named {name!r}; available: {', '.join(available())}")
    prereqs.ensure_container()
    # The builder VM must be running before its network's gateway exists on the host.
    runtime.builder_start()
    gateway = runtime.network_gateway()
    asyncio.run(_build_via_proxy(CONTAINERS / name, tag(name), gateway, no_cache))
    return tag(name)


async def _build_via_proxy(context: Path, image: str, gateway: str, no_cache: bool) -> None:
    server = await egress.EgressProxy(f"build {image}", egress.Policy()).start(gateway, 0)
    port = server.sockets[0].getsockname()[1]
    proxy = f"http://{gateway}:{port}"
    # BuildKit predefines these args: they reach RUN steps but aren't stored in the image,
    # and changing them (e.g. a new port per build) doesn't invalidate the cache.
    build_args = {key: proxy for key in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY")}
    async with server:
        await runtime.build(context, image, build_args, no_cache)
