"""The mode switch: two renames and a symbolic-ref, and no file moves."""

import os

from .errors import Refused
from .gitcmd import git, rev, run
from .state import Repo, named


class NoReview(Refused):
    """There is no review here to switch into. Recoverable: start one."""


def switch_mode(repo: Repo, to: str) -> int:
    repo.recover()
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
    session.record(frm)                       # the approved set, before we move it
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
            git("symbolic-ref", "-m", f"curate: {to} mode", "HEAD", target)
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
    session.record(to)
    return 0
