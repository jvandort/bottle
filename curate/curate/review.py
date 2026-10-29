"""`curate review <branch>`: put the review you are in down, and read another.

The one you are in is paused, not ended: it goes back to write mode, which
parks its approvals in their index file and their ref, and a paused fixup
stays where it was, under its session. Then the named branch is checked out
and its review opened in read mode -- begun first, if it has none, as `start`
would. Coming back is the same command with the first branch.

Nothing moves unless all of it can. Changes in your files that the working
line you are leaving does not have -- edited, staged or new -- would travel
with you onto the other branch and show up as unread there, so they refuse
the switch, and you are left exactly where you were. Every question is asked
before anything moves, too, so cancelling one changes nothing.
"""

from pathlib import Path

from .branches import branch_name, fetch, local_for, remotes, switch_to, to_review
from .errors import Refused
from .gitcmd import rev, run
from .modes import switch_mode
from .pick import choose
from .start import begin, prepare
from .state import Repo, named, short
from .ui import ask_for_base, interactive, uninterrupted


def cmd_review(repo: Repo, argv: list[str]) -> int:
    from_arg = None
    names = []
    i = 0
    while i < len(argv):
        if argv[i] == "--from":
            i += 1
            from_arg = argv[i] if i < len(argv) else None
            if from_arg is None:
                raise Refused("--from needs the base the clean line starts at")
        else:
            names.append(argv[i])
        i += 1
    if len(names) > 1:
        raise Refused(f"unexpected argument: {names[1]}")

    if names:
        chosen = resolve(names[0])
    elif interactive():
        fetch(None)
        rows = to_review(repo)
        if not rows:
            raise Refused("no branches to review")
        cursor = next((n for n, r in enumerate(rows) if r.value == repo.head), 0)
        chosen = choose(["Review which branch?", ""], rows, cursor=cursor,
                        cancelled="cancelled; nothing was changed")
    else:
        raise Refused("which branch? `curate review <branch>`. "
                      "To read the review you are in, `curate read`.")
    if chosen in repo.clean_lines():
        raise Refused(f"{named(chosen)} is a clean line; review the branch it was made from")
    local, tip = local_for(chosen, again="curate review")

    moving = local != repo.head
    if moving:
        pending = unwritten(repo)
        if pending:
            shown = ", ".join(pending[:5]) + (f" and {len(pending) - 5} more" if len(pending) > 5 else "")
            raise Refused(f"your files have changes the working line doesn't: {shown}.\n"
                          f"Commit them in write mode first; nothing has moved.")

    target = next((s for s in repo.sessions() if s.working == local), None)
    beginning = None
    if target is None:
        # Asked now, before anything moves.
        base = from_arg or (ask_for_base(tip, repo.clean_lines()) if interactive() else None)
        if base is None:
            raise Refused(f"{named(local)} has no review yet. Where does its clean line start? "
                          f"`curate review {short(local)} --from <base>`")
        beginning = prepare(repo, local, base_arg=base)
    elif from_arg:
        raise Refused(f"{named(local)} has a review already; --from begins one")

    leaving = repo.session()
    was_reading = leaving is not None and repo.mode == "review"
    with uninterrupted():
        if moving:
            if was_reading:
                switch_mode(repo, "write")          # parks the approvals: the review is paused
            try:
                switch_to(local, chosen, tip)
            except Refused:
                if was_reading:
                    switch_mode(repo, "review")
                raise
            repo.recover()                          # HEAD names the review now, or none
        if beginning is not None:
            begin(repo, beginning, opening=True)
        switch_mode(repo, "review")
    paused = f"; the review of {named(leaving.working)} is paused" if moving and leaving else ""
    print(f"curate: reading {named(local)}{paused}.")
    return 0


def resolve(name: str) -> str:
    """The branch `name` means: a local one, a remote one by its full name
    (origin/x), or the one remote branch called that."""
    for ref in (f"refs/heads/{name}", f"refs/remotes/{name}"):
        if rev(ref) is not None:
            return ref
    known = remotes()
    refs = run(["for-each-ref", "--format=%(refname)", "refs/remotes/"]).stdout.split()
    matches = [r for r in refs if not r.endswith("/HEAD") and branch_name(r, known) == name]
    if len(matches) == 1:
        return matches[0]
    if matches:
        raise Refused(f"{name} is on more than one remote: "
                      f"{', '.join(short(m) for m in matches)}. Name one.")
    raise Refused(f"no branch named {name}")


def unwritten(repo: Repo) -> list[str]:
    """What your files have that the working line does not: edits, staged or
    not, deletions and untracked files. In write mode that is `git status`; in
    read mode the live index is the approved set, so the working line is read
    into an index of its own to compare against."""
    session = repo.session()
    if session is None or repo.mode != "review":
        out = run(["status", "--porcelain=v1", "-z", "--untracked-files=normal"]).stdout
        return [entry[3:] for entry in out.split("\0") if entry]
    scratch = repo.state / "unwritten.index"
    try:
        repo.index_of(session.working, scratch)
        run(["update-index", "--refresh", "-q"], index=scratch)
        changed = run(["diff-files", "--name-only", "-z"], index=scratch).stdout.split("\0")
        new = run(["ls-files", "--others", "--exclude-standard", "-z"], index=scratch).stdout.split("\0")
    finally:
        Path(scratch).unlink(missing_ok=True)
    return sorted(p for p in changed + new if p)
