"""apt repositories and packages, as bottle knows them: Debian's, which every bottle's image has (see
images.DEBIAN), and those features declare (see features.py), installed in an image in one step.

A feature's repositories are installed in the image, its key and its sources,
so a bottle can install more from them. An image's build installs every
repository its features declare, then their packages, together: one
`apt-get update` and one `apt-get install` (install_script), before any
feature's install.sh, which is left with what's its own to do.

bottled keeps what's downloaded from a repository, for every image built and
every bottle running (see cache.py): its packages, the .deb files under its
URI, whose names carry their versions, so a file at a path never changes.
Its indexes (InRelease, Packages) aren't kept: they're how apt learns what's
new. A package is kept whatever its server says about keeping it (some say
max-age=0 of everything). apt checks every package against the repository's signed index, so a
plain-http repository is kept as safely as an https one; bottled fetches
what it keeps over https regardless.
"""

import re
import shlex
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

from bottle import cache

# The ports bottled answers a repository's requests on, by its URI's scheme.
PORTS = {"http": cache.HTTP_PORT, "https": cache.HTTPS_PORT}


@dataclass(frozen=True)
class Repository:
    uri: str  # http:// or https://, to the repository's root (where dists/ and pool/ are)
    suite: str = ""  # e.g. stable, or trixie
    components: tuple[str, ...] = ()  # e.g. main
    key: str = ""  # the file of the key it's signed with, in its feature's directory

    @property
    def host(self) -> str:
        return urlsplit(self.uri).hostname or ""

    @property
    def https(self) -> bool:
        return urlsplit(self.uri).scheme == "https"

    def cached(self) -> cache.Cached:
        """How its downloads are kept: the .deb files under its URI, on its scheme's port."""
        url = urlsplit(self.uri)
        return cache.Cached(url.hostname or "", include=(f"{url.path.rstrip('/')}/*.deb",),
                            ports=(PORTS[url.scheme],), immutable=True)


# Debian's, as debian:trixie-slim's /etc/apt/sources.list.d/debian.sources has them.
DEBIAN = (Repository("http://deb.debian.org/debian"), Repository("http://deb.debian.org/debian-security"))


def cached(repositories: tuple[Repository, ...] | list[Repository]) -> list[cache.Cached]:
    """How `repositories`' downloads are kept: one entry a host and port, its repositories' paths together."""
    merged: dict[tuple[str, tuple[int, ...]], cache.Cached] = {}
    for kept in (r.cached() for r in repositories):
        key = (kept.host, kept.ports)
        before = merged.get(key)
        merged[key] = kept if before is None else cache.Cached(kept.host, include=before.include + kept.include,
                                                                 ports=kept.ports, immutable=True)
    return list(merged.values())


def install_script(repositories: list[tuple[str, Repository]], packages: list[str]) -> str:
    """The shell that installs `repositories` (each with where its key is in the build), then `packages`.

    Each repository's key goes in /etc/apt/keyrings, by the name of its file,
    and its sources in /etc/apt/sources.list.d, signed by it. A build's https
    repositories are answered by bottle-cli from its cache, behind a certificate
    from the caching CA builds have (images.BUILDS_CA), in $BOTTLE_BUILD_CA: apt trusts it for
    their hosts alone, from a file in /run, and nothing of it stays in the image.
    """
    lines = ["set -eu"]
    for at, repository in repositories:
        keyring = f"/etc/apt/keyrings/{Path(at).name}"
        source = f"deb [signed-by={keyring}] {repository.uri} {repository.suite} {' '.join(repository.components)}"
        lines += [f"install -D -m 0644 {shlex.quote(at)} {shlex.quote(keyring)}",
                  f"echo {shlex.quote(source)} >> /etc/apt/sources.list.d/{_list_name(repository)}.list"]
    https = sorted({r.host for _, r in repositories if r.https})
    lines += ['options=""', 'if [ -n "${BOTTLE_BUILD_CA:-}" ]; then',
              '  printf \'%s\\n\' "$BOTTLE_BUILD_CA" > /run/bottle-build-ca.pem']
    lines += [f'  options="$options -o Acquire::https::{host}::CaInfo=/run/bottle-build-ca.pem"' for host in https]
    lines += ["fi",
              "# shellcheck disable=SC2086 # word splitting is intended",
              "apt-get $options update",
              f"apt-get $options install -y --no-install-recommends {' '.join(shlex.quote(p) for p in packages)}",
              "rm -rf /var/lib/apt/lists/* /run/bottle-build-ca.pem"]
    return "\n".join(lines) + "\n"


def _list_name(repository: Repository) -> str:
    """Its sources' file name: its host and path, as a name."""
    url = urlsplit(repository.uri)
    return re.sub(r"[^A-Za-z0-9.-]+", "-", f"{url.hostname}{url.path}").strip("-")
