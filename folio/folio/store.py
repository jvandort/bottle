"""Where folio keeps what it knows, and how it writes it so a reader never sees half.

    $FOLIO_HOME (default ~/.folio)/
      repos/<name>/              the repos folio owns: the broker's vault (see broker.py)
      <repo key>/                one per repo (see repos.py)
        repo.json                the repo's path and name
        spec.json                the repo's bottle spec, canonical
        <thread>/
          current -> <slot>/     the thread's state as it is now
          <slot>/                thread.json, and later the snapshot archives

A thread's state is a set of files that must change together, so it's
written as a whole new slot, named by the time it was written, and `current`
is moved onto it by renaming a symlink over it, which is atomic. Files the
change doesn't touch are hardlinked from the old slot. A reader that follows
`current` sees one whole slot or the other, never a mix; a crash leaves
`current` as it was. Any slot `current` doesn't point to is left over, and the
next write removes it, so a thread never has more than two.
"""

import json
import os
import re
import shutil
import time
from pathlib import Path

CURRENT = "current"
SLOT = re.compile(r"\d{8}T\d{6}\.\d{3}Z")  # a slot's name: when it was written (_new_slot)


def folio_home() -> Path:
    return Path(os.environ.get("FOLIO_HOME") or Path.home() / ".folio")


def read_json(path: Path) -> dict | None:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return None


def write_json(path: Path, data: dict) -> None:
    """Replace `path` atomically with `data`, as JSON."""
    write_bytes(path, (json.dumps(data, indent=2) + "\n").encode())


def write_bytes(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    staging = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    staging.write_bytes(content)
    staging.replace(path)


def current_slot(directory: Path) -> Path | None:
    """The slot `current` points at, or None if nothing has been written."""
    link = directory / CURRENT
    return directory / os.readlink(link) if link.is_symlink() else None


def write_slot(directory: Path, files: dict[str, bytes | Path | None]) -> Path:
    """Write a new slot in `directory` with `files`, and every other file of the current slot; make it current.

    A file is given as its content, or as a file to move in (from the same
    filesystem: an archive streamed in beside the slots, say), or as None to
    leave it out of the new slot.
    """
    directory.mkdir(parents=True, exist_ok=True)
    old = current_slot(directory)
    _remove_leftovers(directory, keep=old)
    slot = _new_slot(directory)
    for name, content in files.items():
        if isinstance(content, Path):
            content.replace(slot / name)
        elif content is not None:
            (slot / name).write_bytes(content)
    if old is not None:
        for kept in old.iterdir():
            if kept.name not in files:
                os.link(kept, slot / kept.name)
    staging = directory / f".{CURRENT}.{os.getpid()}.tmp"
    staging.unlink(missing_ok=True)
    staging.symlink_to(slot.name)
    staging.replace(directory / CURRENT)
    if old is not None:
        shutil.rmtree(old, ignore_errors=True)
    return slot


def _new_slot(directory: Path) -> Path:
    while True:
        now = time.time()
        name = time.strftime("%Y%m%dT%H%M%S", time.gmtime(now)) + f".{int(now * 1000) % 1000:03d}Z"
        try:
            (directory / name).mkdir()
            return directory / name
        except FileExistsError:
            time.sleep(0.001)


def _remove_leftovers(directory: Path, keep: Path | None) -> None:
    """Slots `current` doesn't point to: what a crash mid-write leaves behind.

    Only slots, by their names: anything else beside them -- a thread's notes,
    the dot-named work in progress of staging() -- is left alone.
    """
    for entry in directory.iterdir():
        if entry.is_dir() and not entry.is_symlink() and entry != keep and SLOT.fullmatch(entry.name):
            shutil.rmtree(entry, ignore_errors=True)


def staging(directory: Path) -> Path:
    """A fresh directory beside the slots, for files about to be moved into one.

    On the same filesystem, so moving them in is a rename. One per process; a
    crashed one is cleared the next time the same pid comes round, which is
    harmless, and `folio delete` takes the whole thread directory anyway.
    """
    path = directory / f".staging.{os.getpid()}"
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)
    return path
