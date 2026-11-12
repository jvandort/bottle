"""apt repositories, as bottle knows them: Debian's, which every bottle's image has (see images.DEBIAN).

bottled keeps what's downloaded from a repository, for every image built and
every bottle running (see cache.py): its packages, the .deb files under its
URI, whose names carry their versions, so a file at a path never changes.
Its indexes (InRelease, Packages) aren't kept: they're how apt learns what's
new. apt checks every package against the repository's signed index, so a
plain-http repository is kept as safely as an https one; bottled fetches
what it keeps over https regardless.
"""

from dataclasses import dataclass
from urllib.parse import urlsplit

from bottle import cache

# The ports bottled answers a repository's requests on, by its URI's scheme.
PORTS = {"http": cache.HTTP_PORT, "https": cache.HTTPS_PORT}


@dataclass(frozen=True)
class Repository:
    uri: str  # http:// or https://, to the repository's root (where dists/ and pool/ are)

    def cached(self) -> cache.Cached:
        """How its downloads are kept: the .deb files under its URI, on its scheme's port."""
        url = urlsplit(self.uri)
        return cache.Cached(url.hostname or "", include=(f"{url.path.rstrip('/')}/*.deb",),
                            ports=(PORTS[url.scheme],))


# Debian's, as debian:trixie-slim's /etc/apt/sources.list.d/debian.sources has them.
DEBIAN = (Repository("http://deb.debian.org/debian"), Repository("http://deb.debian.org/debian-security"))


def cached(repositories: tuple[Repository, ...] | list[Repository]) -> list[cache.Cached]:
    """How `repositories`' downloads are kept: one entry a host and port, its repositories' paths together."""
    merged: dict[tuple[str, tuple[int, ...]], cache.Cached] = {}
    for kept in (r.cached() for r in repositories):
        key = (kept.host, kept.ports)
        before = merged.get(key)
        merged[key] = kept if before is None else cache.Cached(kept.host, include=before.include + kept.include,
                                                                 ports=kept.ports)
    return list(merged.values())
