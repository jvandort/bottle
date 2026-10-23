"""The HTTP cache: files bottles download, kept on the machine for the next bottle.

Every bottle starts from its image, so a build in a new one downloads every
dependency again. A feature can name hosts whose downloads are worth keeping,
and bottled terminates the bottle's TLS to them as it does for a credential's host (see ca.py and
egress.py), answers what it has from here, and keeps what it fetches.

The store is the files themselves, at $BOTTLE_HOME/cache/http/<host>/<path>,
the path as the bottle asked for it: no headers and no status, since only a
200 is kept and a 200 is all a hit answers. A redirect is followed here, and
what it led to is kept under the path that was asked for. What's kept is
never refreshed, which suits hosts whose files don't change once they're
published; a feature excludes the paths that do, such as an index of a
package's versions. A file's modification time is when a bottle last
used it, and one unused for EXPIRY is removed (Store.expire): long enough for
the threads of a day's work on one project to share their downloads.

Shared by every bottle, and so kept only from requests whose answer can't
depend on who asked: no credential, no cookie, no query, no range, and the
request bottled sends for it is its own, not the bottle's headers.
"""

import fcntl
import fnmatch
import os
import re
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

from bottle.store import bottle_home

HOST = re.compile(r"(\*\.)?[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*")
# Where files are written before they're complete; a hidden name, which no host has.
PARTIAL = ".partial"
# A partial file this old was left by a bottled that stopped mid-download.
PARTIAL_EXPIRY = 24 * 60 * 60
# A kept file unused for this long is removed.
EXPIRY = 7 * 24 * 60 * 60


@dataclass(frozen=True)
class Cached:
    """A host whose downloads bottled keeps, but for paths matching `exclude`."""

    host: str  # a name, or `*.` and a name
    exclude: tuple[str, ...] = ()  # fnmatch patterns over the path, e.g. "*/index.json"

    def matches(self, host: str) -> bool:
        return fnmatch.fnmatch(host.lower(), self.host)

    def excludes(self, path: str) -> bool:
        return any(fnmatch.fnmatchcase(path, pattern) for pattern in self.exclude)


def cached_for(specs: tuple[str, ...] | list[str]) -> list[Cached]:
    """The hosts a bottle with these features has its downloads kept for."""
    from bottle import features  # which imports this module

    return [Cached(host.lower(), f.cache.exclude) for f in features.resolve(list(specs)) if f.cache for host in f.cache.hosts]


class Store:
    """The files, by host and path."""

    def __init__(self, root: Path | None = None) -> None:
        self.root = root or bottle_home() / "cache" / "http"

    def file(self, host: str, path: str) -> Path | None:
        """Where the file for `path` on `host` is kept; None for a path no file can be kept at.

        That's one that isn't a file's (a directory's, ending in /), and one
        the filesystem would read as another: `.`, `..`, or `//`.
        """
        host = host.lower()
        if not HOST.fullmatch(host) or host.startswith("*"):
            return None
        segments = path.split("/")
        if segments[0] != "" or len(segments) < 2:
            return None
        for segment in segments[1:]:
            if segment in ("", ".", "..") or "\0" in segment or len(os.fsencode(segment)) > 255:
                return None
        return self.root.joinpath(host, *segments[1:])

    def open(self, host: str, path: str) -> BinaryIO | None:
        """The file kept for `path` on `host`, open for reading; None if there isn't one."""
        file = self.file(host, path)
        if file is None:
            return None
        try:
            opened = open(file, "rb")
        except OSError:  # not there, a directory, or under a file
            return None
        if not self._named(opened.fileno(), file):
            opened.close()
            return None
        try:
            os.utime(opened.fileno())  # used now: its modification time is when it was last used
        except OSError:
            pass
        return opened

    def partial(self, host: str, path: str) -> "Partial | None":
        """A file for `path` on `host` to write, kept when it's committed; None if it can't be kept."""
        file = self.file(host, path)
        if file is None:
            return None
        directory = self.root / PARTIAL
        directory.mkdir(parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(dir=directory)
        return Partial(self, file, Path(temporary), os.fdopen(fd, "wb"))

    def expire(self) -> None:
        """Remove the files no bottle has used for EXPIRY, and the directories that leaves empty.

        And partial files a bottled left when it stopped before finishing
        them. A file a bottle is being sent as it's removed is still sent whole.
        """
        now = time.time()
        for directory, _, files in os.walk(self.root, topdown=False):
            expiry = PARTIAL_EXPIRY if Path(directory) == self.root / PARTIAL else EXPIRY
            for name in files:
                file = os.path.join(directory, name)
                try:
                    if now - os.stat(file).st_mtime > expiry:
                        os.unlink(file)
                except OSError:
                    pass
            if Path(directory) not in (self.root, self.root / PARTIAL):
                try:
                    os.rmdir(directory)
                except OSError:  # not empty
                    pass

    def _named(self, fd: int, file: Path) -> bool:
        """Whether what `fd` has open is `file`, letter for letter.

        On a filesystem that ignores case (a Mac's, by default) `Foo.tgz`
        opens `foo.tgz`, which is another file to a server. The name it has on
        disk is asked for (F_GETPATH, macOS only); elsewhere, names are taken
        to be exact.
        """
        getpath = getattr(fcntl, "F_GETPATH", None)
        if getpath is None:
            return True
        try:
            named = os.fsdecode(fcntl.fcntl(fd, getpath, bytes(1024)).split(b"\0", 1)[0])
        except OSError:
            return False
        return named.endswith(os.sep + str(file.relative_to(self.root)))


class Partial:
    """A file being downloaded: written to one side, and put in its place only when complete."""

    def __init__(self, store: Store, file: Path, temporary: Path, out: BinaryIO) -> None:
        self.store, self.file, self.temporary, self.out = store, file, temporary, out

    def write(self, data: bytes) -> None:
        self.out.write(data)

    def commit(self) -> bool:
        """Keep the file; False if it can't be, where something else already is or would be read as it."""
        self.out.close()
        try:
            self.file.parent.mkdir(parents=True, exist_ok=True)
            # Where a directory was already there under another case, this file would be found under that name.
            fd = os.open(self.file.parent, os.O_RDONLY)
            try:
                named = self.store._named(fd, self.file.parent)
            finally:
                os.close(fd)
            if named:
                self._unlink_other_case()
                os.replace(self.temporary, self.file)
                return True
        except OSError:  # a file where a directory goes, or a directory where the file does
            pass
        self.discard()
        return False

    def _unlink_other_case(self) -> None:
        """Remove a file already kept under this name in another case, which a rename might leave its name to."""
        try:
            fd = os.open(self.file, os.O_RDONLY)
        except OSError:
            return
        try:
            other = not self.store._named(fd, self.file)
        finally:
            os.close(fd)
        if other:
            self.file.unlink()

    def discard(self) -> None:
        self.out.close()
        self.temporary.unlink(missing_ok=True)
