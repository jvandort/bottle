"""bottle.host: what a bottle can ask of folio serve, at http://bottle.host/, and the remotes it's served there.

A bottle's machine routes bottle.host (ADDRESS) to folio serve, which answers it (folio-cli's serve/host.rs): each
mount's remotes, at /git/<mount>/<remote>, over git's smart HTTP protocol, each a view of the hub (the repo itself,
on the controller): the upstream, the hub's own branches, or one git namespace, which is all a push can write
(serve/git.rs); messages to other bottles (serve/messages.rs); and, with the list-bottles permission, the other
running bottles. The rest of TEST-NET-1 (SSH_NETWORK) is the bottles a bottle may ssh to, an address each.
"""

import re
from dataclasses import dataclass
from pathlib import Path

from folio.bottle.errors import BottleError

HOST = "bottle.host"
# Where a bottle reaches bottle.host: its /etc/hosts says so. A documentation address (TEST-NET-1) that nothing
# real uses, which the bottle's machine routes to folio serve (services.network).
ADDRESS = "192.0.2.1"
# Where a bottle reaches the bottles it may ssh to (a thread's, its sidecar): the rest of TEST-NET-1, an address
# for each, under the name its creator gave it.
SSH_NETWORK = "192.0.2.0/24"
SSH_PORT = 22
MAX_SSH = 250
# Where a bottle's remotes are: http://bottle.host/git/<mount>/<remote>.
GIT_PATH = "/git"


def ssh_address(index: int) -> str:
    """The address of a bottle's `index`th ssh grant: 192.0.2.2 onwards."""
    return f"192.0.2.{2 + index}"


# What a remote's fetch can see: the upstream (served as origin is), the repo's
# own branches (as host is), or one namespace, NS + its name.
UPSTREAM, HEADS, NS = "upstream", "heads", "ns/"
# Remote and namespace names: one path segment, so a remote is one URL segment
# and a namespace is flat. Namespaces nest rather than concatenate, so `a/b`
# would mean refs/namespaces/a/refs/namespaces/b/, not what anyone expects.
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


@dataclass(frozen=True)
class Remote:
    """One remote a bottle is served: http://bottle.host/git/<name>."""

    name: str
    fetch: str  # UPSTREAM, HEADS, or NS + a namespace
    push: str | None = None  # NS + the namespace pushes go into; None: read-only

    @property
    def fetch_namespace(self) -> str | None:
        return self.fetch.removeprefix(NS) if self.fetch.startswith(NS) else None

    @property
    def push_namespace(self) -> str | None:
        return self.push.removeprefix(NS) if self.push else None

    @property
    def spec(self) -> str:
        """As parse takes it: work:fetch=ns/x-in,push=ns/x-out."""
        return f"{self.name}:fetch={self.fetch}" + (f",push={self.push}" if self.push else "")

    @classmethod
    def parse(cls, spec: str) -> "Remote":
        """A remote from its spec: NAME:fetch=VIEW[,push=ns/NAMESPACE]."""
        usage = (f"invalid remote {spec!r}: use NAME:fetch=VIEW[,push=ns/NAMESPACE], "
                 f"VIEW being {UPSTREAM}, {HEADS} or ns/NAMESPACE")
        name, colon, rest = spec.partition(":")
        if not colon or not NAME.fullmatch(name):
            raise BottleError(usage)
        settings: dict[str, str] = {}
        for item in rest.split(","):
            key, equals, value = item.partition("=")
            if not equals or key not in ("fetch", "push") or key in settings:
                raise BottleError(usage)
            settings[key] = value
        fetch, push = settings.get("fetch"), settings.get("push")
        if fetch is None or not (fetch in (UPSTREAM, HEADS) or _namespace_view(fetch)):
            raise BottleError(usage)
        if push is not None and not _namespace_view(push):
            raise BottleError(f"invalid remote {spec!r}: push takes ns/NAMESPACE, the namespace pushes go into")
        return cls(name, fetch, push)


def _namespace_view(view: str) -> bool:
    return view.startswith(NS) and NAME.fullmatch(view.removeprefix(NS)) is not None


@dataclass(frozen=True)
class Served:
    """One repo a bottle is served -- a mount's -- with its remotes, at http://bottle.host/git/<at>/<remote>."""

    git_dir: Path | None  # the repo its remotes are served from; None: it's gone
    remotes: tuple[Remote, ...] = ()
    # Where under /git its remotes are: the mount's path, without the leading /; "" puts them at /git/<remote>.
    at: str = ""

    def served(self) -> dict[str, Remote]:
        return {remote.name: remote for remote in self.remotes}
