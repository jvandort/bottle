"""The mode switch: two renames and a symbolic-ref, and no file moves."""

import os

from .errors import Refused
from .gitcmd import git, rev, run
from .state import Repo, named, shown


class NoReview(Refused):
    """There is no review here to switch into. Recoverable: start one."""


def switch_mode(repo: Repo, to: str) -> int:
    # The one place this is checked. Every command starts consistent, since
    # cli re-anchors on HEAD first; this is for the switches commands make
    # part way through, after git has run, right before the indexes move.
    repo.require_consistent()
    session = repo.session()
    if session is None:
        raise NoReview("no review here. "
                       "Start one with `curate start --from <base>`.")
    frm = repo.mode
    if frm == to:
        session.record(frm)
        return 0

    target = session.clean if to == "review" else session.working
    if rev(target) is None:
        raise Refused(f"{named(target)} does not exist")
    # The approved set, before we move it. Only reading can have changed it
    # since it was last recorded; in write mode it sits parked, untouched.
    if frm == "review":
        session.record(frm)
    if not session.index(to).exists():
        session.rebuild(to)

    live = repo.gitdir / "index"
    lock = repo.gitdir / "index.lock"
    # Git's own lock, so an editor running git in the background cannot race us.
    try:
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
    except FileExistsError:
        raise Refused("another git process holds .git/index.lock; try again")
    parked = session.index(frm if frm in ("write", "review") else "write")
    try:
        os.close(fd)
        if live.exists():
            live.replace(parked)
        session.index(to).replace(live)       # renames: atomic, and free
        try:
            git("symbolic-ref", "-m", f"curate: {shown(to)} mode", "HEAD", target)
        except Refused:
            # Every other failure here leaves HEAD and the recorded mode
            # disagreeing, which the invariant catches. This one would not:
            # HEAD and the mode would still agree with each other and with
            # nothing else, while the live index had already become the other
            # mode's. Undo the renames rather than leave that behind.
            live.replace(session.index(to))
            if parked.exists():
                parked.replace(live)
            raise
    finally:
        lock.unlink(missing_ok=True)

    run(["update-index", "--refresh", "-q"])  # rebuild the stat cache; no files move
    repo.set_state(to, session)
    # Not recorded again: a switch moves the approved set, it never changes it.
    return 0
