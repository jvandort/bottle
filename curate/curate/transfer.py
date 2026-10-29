"""`curate transfer`: carry the review you are in over to a branch that replaced
its working line.

Five steps, the first four of which write nothing you would have to undo:
fetch, choose the new line, choose its base, check nothing is in the way --
and only then switch to it and copy the review, coming back in the mode you
left. Cancelling either list leaves everything as it was.
"""

from .branches import (branches, fetch, local_for, remote_of, remotes,
                       switch_to)
from .carry import choose_base, names_for
from .errors import Refused
from .gitcmd import git, run
from .modes import switch_mode
from .pick import Row, choose
from .start import begin, prepare
from .state import Repo, Session, named, short
from .ui import interactive, uninterrupted


def remotes_to_fetch(source: Session, head: str) -> list[str] | None:
    """The working line's remote, and the old base's. None means all of them:
    with no remote to go on, it is not obvious which one has the new line."""
    first = remote_of(source.working)
    if first is None:
        return None
    wanted = [first]
    for name in names_for(source.base, source, head):
        branch = name.split("~")[0].split("^")[0]
        if source.repo.objects.rev(f"refs/heads/{branch}") is not None:
            r = remote_of(branch)
        else:
            r = next((x for x in remotes() if branch.startswith(f"{x}/")), None)
        if r and r not in wanted:
            wanted.append(r)
    return wanted


def choose_line(source: Session, fetched: list[str] | None) -> str:
    up = git("rev-parse", "--symbolic-full-name",
             f"{short(source.working)}@{{upstream}}", check=False)
    skip = {source.working, up} | source.repo.clean_lines()
    if fetched is None:
        rows = branches("refs/remotes/", skip=skip)
    else:
        rows = [r for remote in fetched[:1]
                for r in branches(f"refs/remotes/{remote}/", skip=skip)]
    rows.append(Row(["<show local>"],
                    expand=lambda: branches("refs/heads/", skip=skip) or
                    [Row(["(no other local branches)"])]))
    header = [f"Transfer the review of {named(source.working)} to:", ""]
    return choose(header, rows, cursor=0)


def cmd_transfer(repo: Repo, argv: list[str]) -> int:
    if len(argv) > 1:
        raise Refused("usage: curate transfer [<branch>]")
    source = repo.session()
    if source is None:
        raise Refused("no review here to transfer. `curate list` shows them.")
    if source.fixup_pending:
        raise Refused("a fixup is paused on a conflict. Finish it or abort it "
                      "before transferring the review.")

    which = remotes_to_fetch(source, source.working)
    fetch(which)
    if argv:
        chosen = (f"refs/heads/{argv[0]}" if repo.objects.rev(f"refs/heads/{argv[0]}") else
                  git("rev-parse", "--symbolic-full-name", argv[0], check=False))
        if not chosen.startswith(("refs/heads/", "refs/remotes/")):
            raise Refused(f"no branch {argv[0]}")
    elif interactive():
        chosen = choose_line(source, which)
    else:
        raise Refused("which branch? `curate transfer <branch>`")

    local, tip = local_for(chosen, repo.objects, "curate transfer")
    if local == source.working:
        raise Refused(f"that is {named(local)}, the branch under review already")
    if local in repo.clean_lines():
        raise Refused(f"{named(local)} is the clean line of a review")
    if any(s.working == local for s in repo.sessions()):
        raise Refused(f"{named(local)} has a review already: "
                      f"`git switch {short(local)} && curate read`")
    # Everything start would check, and the copy itself, before anything
    # moves: a copy refused after the switch would leave you on the new branch
    # with no review to transfer again.
    beginning = prepare(repo, local, base_arg=choose_base(source, tip),
                        source=source)

    # Every question is answered. From here on, things change, and stopping
    # between the switch and the copy would leave the same -- so nothing stops it.
    with uninterrupted():
        mode = repo.mode
        if mode == "review":
            switch_mode(repo, "write")
        try:
            switch_to(local, chosen, tip, repo.objects)
        except Refused:
            # The branch is back as it was; put the review back with it.
            if mode == "review":
                switch_mode(repo, "review")
            raise
        begin(repo, beginning)
        if mode == "review":
            switch_mode(repo, "review")
        return 0
