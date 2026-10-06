"""The anchor: an idle container on every bottle network, so no network's bridge is torn down while it exists.

apple/container gives a network a bridge on this machine (bridge100,
bridge101, ...) when its first VM attaches, and destroys it when its last one
detaches. The network remembers which bridge it had, though, and takes the same
one when it starts again, even if another network has been given it since:
that network's bridge is reconfigured out from under it, and its VMs, still
running, are cut off. So restarting one bottle could cut off another
(apple/container#2051).

The anchor keeps every network's bridge for as long as the network exists, so
no bridge is freed for another network to take. It's one container for all of
them, about 290 MB of the machine's memory. A container's networks are fixed
when it starts, so when a network comes or goes the anchor is replaced, not
changed: the new one starts before the old one goes, so no network is ever
left without one. The default network, which the image builder uses, is
anchored too whenever any bottle is.
"""

import fcntl
import logging
import uuid
from contextlib import contextmanager

from bottle import runtime
from bottle.store import bottle_home, namespace

log = logging.getLogger("bottle.anchor")

IMAGE = "docker.io/library/alpine:3"
LABEL = "bottle.anchor"
DEFAULT_NETWORK = "default"


def ensure(networks: set[str]) -> None:
    """Have one running anchor on exactly `networks` (those that exist) and the default network; none if there are none.

    A no-op when the running anchor is already on them. Serialized across
    bottle processes, so two never replace the anchor at once.
    """
    with _lock():
        existing = runtime.networks()
        wanted = networks & existing
        if wanted:
            wanted |= {DEFAULT_NETWORK} & existing
        anchors = _anchors()
        keep = next(
            (name for name, info in anchors.items() if info.state == "running" and set(info.networks) == wanted), None
        ) if wanted else None
        if wanted and keep is None:
            keep = f"{_prefix()}{uuid.uuid4().hex[:12]}"
            runtime.idle_run(keep, IMAGE, sorted(wanted), {LABEL: "1"})
            log.info("anchor %s on %s", keep, ", ".join(sorted(wanted)))
        for name in anchors.keys() - {keep}:
            runtime.container_delete(name)
            log.info("anchor %s removed", name)


def remove() -> None:
    """Remove every anchor."""
    ensure(set())


def _anchors() -> dict[str, runtime.ContainerInfo]:
    """This home's anchors: named and labelled as one."""
    return {
        name: info for name, info in runtime.containers().items()
        if name.startswith(_prefix()) and LABEL in info.labels
    }


def _prefix() -> str:
    # Not bottle-...: that's where bottles' containers are named.
    return f"bottled-{namespace()}anchor-"


@contextmanager
def _lock():
    path = bottle_home() / "anchor.lock"
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with open(path, "w") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        yield
