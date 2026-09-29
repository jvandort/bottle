"""Reporting on reviews, and forgetting them."""

import shutil

from .errors import Refused
from .gitcmd import rev, run
from .modes import switch_mode
from .state import Repo, named, short


def problems(repo: Repo) -> list[str]:
    """What is wrong that curate cannot put right by itself.

    Deliberately short. A malformed version stamp is refused before anything
    gets this far, and everything derived -- index caches, hooks naming a
    curate that moved, an older state layout -- repairs itself on any command.
    What is left is damage done to git: a branch a review needs, deleted.
    """
    wrong: list[str] = []
    for s in repo.sessions():
        # Both are recoverable: deleting a branch takes its reflog, but commits made
        # on it are still in HEAD's, and the objects outlive the ref until gc.
        for line, name in ((s.clean, "clean"), (s.working, "working")):
            if rev(line) is not None:
                continue
            other = s.working if name == "clean" else s.clean
            wrong.append(
                f"{short(other)}: its {name} line {named(line)} no longer "
                f"exists.\n`git reflog` still has its commits, so `git branch "
                f"{short(line)} <oid>` puts it back;\n`curate drop "
                f"{short(s.working)}` forgets the review instead. Either way "
                f"what you approved is safe in {s.ref}.")
    return wrong


def cmd_status(repo: Repo, argv: list[str]) -> int:
    porcelain = "--porcelain" in argv
    wrong = problems(repo)
    session = repo.session()
    consistent = repo.consistent()
    mode = repo.mode if consistent else "away"
    fields = {"mode": mode}
    if session is not None:
        fields.update({
            "clean": session.clean,
            "working": session.working,
            "fixup": "pending" if session.fixup_pending else "none",
        })
    fields["consistent"] = "yes" if consistent else "no"
    fields.setdefault("fixup", "none")
    fields["problems"] = str(len(wrong))

    if porcelain:
        for key, value in fields.items():
            print(f"{key}={value}")
        return 0

    if session is None:
        print("curate: no review here. `curate start --from <base>` begins one.")
    else:
        print(f"mode:            {fields['mode']}")
        print(f"working branch:  {short(fields['working'])}")
        print(f"clean branch:    {short(fields['clean'])}")
        if fields["fixup"] == "pending":
            print("fixup:           paused on a conflict -- `curate resolve`, "
                  "then `curate fixup --continue`")
        if not consistent:
            print(f"\nHEAD is {named(repo.head)}, not "
                  f"{named(repo.expected_head())}: something moved it out from "
                  f"under the review.\nThe approved set is safe in {session.ref}.")
        else:
            print("\n`git status` is the review: unstaged is what you have not "
                  "read yet.")
    for line in wrong:
        first, *rest = line.split("\n")
        print(f"WRONG:   {first}")
        for more in rest:
            print(f"         {more}")
    return 0


def cmd_list(repo: Repo, argv: list[str]) -> int:
    sessions = repo.sessions()
    if not sessions:
        print("no reviews")
        return 0
    active = repo.session()
    for s in sessions:
        here = "*" if active and s.slug == active.slug else " "
        paused = "  (fixup paused)" if s.fixup_pending else ""
        print(f"{here} {short(s.working)} -> {short(s.clean)}{paused}")
    return 0


def cmd_drop(repo: Repo, argv: list[str]) -> int:
    if len(argv) != 1:
        raise Refused("usage: curate drop <branch>")
    name = argv[0]
    for s in repo.sessions():
        if name not in (short(s.clean), s.clean, short(s.working), s.working):
            continue
        active = repo.session()
        if active and active.slug == s.slug:
            if repo.mode == "review" and repo.consistent():
                # You cannot drop the review you are standing in: the index
                # being dropped is the live one. Do it rather than refuse.
                switch_mode(repo, "write")
                print(f"curate: left review mode; HEAD is {named(s.working)} again.")
            repo.set_state(repo.mode, None)
        # The clean branch is ours to delete only while it holds nothing. Once
        # something has been approved onto it, it is work, and work stays.
        untouched = s.base and rev(s.clean) == s.base
        shutil.rmtree(s.dir, ignore_errors=True)
        run(["update-ref", "-d", s.ref])
        if untouched:
            run(["branch", "-D", short(s.clean)])
            print(f"dropped the review of {named(s.working)}, and deleted its "
                  f"clean line {named(s.clean)}: nothing had been approved onto it.")
            print(f"`curate start --from <base>` begins again.")
        else:
            print(f"dropped the review of {named(s.working)}. Its clean line, "
                  f"{named(s.clean)}, has commits approved onto it, so the branch "
                  f"is left alone -- `git branch -D {short(s.clean)}` if you want "
                  f"that work gone too.")
        return 0
    raise Refused(f"no review of {name}")
