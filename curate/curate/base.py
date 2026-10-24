"""`curate update-base`: the review you are in, onto a new base, in place.

For a working line rebased where it stands rather than pushed under a new
name. The review's base is still the old one, so everything upstream did
since shows up as unreviewed. This is the copy `transfer` makes, onto the
same branch: the clean line replayed onto the new base, the approvals merged
across, and the new base recorded. Everything is worked out before anything
changes, and nothing in the old clean line or approvals is lost: their old
tips are in the reflogs of the clean line and of the approvals ref.
"""

from .carry import carry, guess, names_for, what_carried
from .errors import Refused
from .gitcmd import git, rev, run
from .modes import switch_mode
from .state import Repo, Session, named, write
from .ui import interactive, pick_base, uninterrupted


def ask(repo: Repo, session: Session) -> str:
    names = names_for(session.base, session, session.working)
    best, via, tip = guess(session.working, names)
    said = ", ".join(names) or "no branch"
    subject = git("log", "-1", "--format=%s", session.base)
    header = [f"The review of {named(session.working)}",
              f"  current base  {session.base[:8]}  ({said})  {subject}", ""]
    return pick_base(session.working, best, via, tip, header=header,
                     marks={session.base: "current base"},
                     hide=repo.clean_lines())


def cmd_update_base(repo: Repo, argv: list[str]) -> int:
    if len(argv) > 1:
        raise Refused("usage: curate update-base [<base>]")
    session = repo.session()
    if session is None:
        raise Refused("no review here. `curate list` shows them.")
    if session.fixup_pending:
        raise Refused("a fixup is paused on a conflict. Finish it or abort it first.")

    if argv:
        base = rev(argv[0])
        if base is None:
            raise Refused(f"cannot resolve {argv[0]}")
    elif interactive():
        base = ask(repo, session)
    else:
        raise Refused("which base? `curate update-base <base>`")
    if base == session.base:
        print(f"curate: the base is {base[:8]} already; nothing to change.")
        return 0

    # Writes objects and no refs, so a refusal here leaves nothing behind.
    carried = carry(repo, session, base)
    old_clean, old_base = rev(session.clean), session.base

    # Every question is answered. From here on, things change -- together.
    with uninterrupted():
        mode = repo.mode
        if mode == "review":
            switch_mode(repo, "write")      # the approvals parked, out of HEAD's way
        git("update-ref", "-m", f"curate update-base: onto {base[:8]}",
            session.clean, carried.tip, old_clean)
        session.base = base
        write(session.dir / "base", base)
        idx = session.index("review")
        tmp = idx.with_suffix(".rebasing")
        repo.index_of(carried.approved, tmp)
        run(["update-index", "--refresh", "-q"], index=tmp)   # non-zero is normal
        tmp.replace(idx)
        session.record("write")
        if mode == "review":
            switch_mode(repo, "review")

    names = names_for(base, session, session.working)
    called = f" ({', '.join(names)})" if names else ""
    print(f"curate: the review of {named(session.working)} is on {base[:8]}{called} "
          f"now, and its clean line at {carried.tip[:8]}.")
    what_carried(carried)
    print(f"        Wrong base? `curate update-base {old_base[:8]}` goes back; "
          f"the clean line was at {old_clean[:8]}.")
    return 0
