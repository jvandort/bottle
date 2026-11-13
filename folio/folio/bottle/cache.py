"""The HTTP cache: which hosts' downloads are kept on the machine, for the next bottle.

Every bottle starts from its image, so a build in a new one downloads every
dependency again. A feature can name hosts whose downloads are worth keeping,
and every apt repository an image has is kept too (see apt.py). bottle-cli,
which carries a bottle's connections, terminates the bottle's TLS to them
with the machine's caching CA, answers what it has, and
keeps what it fetches, in $BOTTLE_HOME/cache/http, removing what's gone
unused for a week (see bottle-agent-client's cache and store). A machine's
policy says which hosts, and how (services.cache_rules).

What's kept is never refreshed, which suits hosts whose files don't change
once they're published; a feature excludes the paths that do, such as an
index of a package's versions. Shared by every bottle, so kept only from
requests whose answer can't depend on who asked: no credential, no cookie,
no query, no range.
"""

import re
from dataclasses import dataclass

HOST = re.compile(r"(\*\.)?[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*")

# Where a cached host is asked: over https, whose TLS bottle-cli terminates, or plain http, which it reads as it is.
HTTPS_PORT = 443
HTTP_PORT = 80


@dataclass(frozen=True)
class Cached:
    """A host whose downloads are kept, asked on `ports`: those at paths matching `include`, if it says any,
    but for paths matching `exclude`; `immutable` if what's at a path never changes, whatever the host says about
    keeping it (an apt repository's packages, which apt checks against its signed index)."""

    host: str  # a name, or `*.` and a name
    exclude: tuple[str, ...] = ()  # fnmatch patterns over the path, e.g. "*/index.json"
    include: tuple[str, ...] = ()  # fnmatch patterns over the path, e.g. "/debian/*.deb"; none: every path
    ports: tuple[int, ...] = (HTTPS_PORT,)
    immutable: bool = False


def cached_for(specs: tuple[str, ...] | list[str]) -> list[Cached]:
    """The hosts a bottle with these features has its downloads kept for: those its features name, and the apt
    repositories its image has (see apt.py)."""
    from folio.bottle import apt, features  # which import this module

    resolved = features.resolve(list(specs))
    named = [Cached(host.lower(), f.cache.exclude) for f in resolved if f.cache for host in f.cache.hosts]
    return named + apt.cached((*apt.DEBIAN, *(r for f in resolved for r in f.apt.repositories)))
