"""`curate transfer`: carry the review you are in over to a branch that replaced
its working line.

Five steps, the first four of which write nothing you would have to undo:
fetch, choose the new line, choose its base, check nothing is in the way --
and only then switch to it and copy the review, coming back in the mode you
left. Cancelling either list leaves everything as it was.
"""

import subprocess
import sys

from .carry import choose_base, names_for
from .errors import Refused
from .gitcmd import environment, git, rev, run
from .modes import switch_mode
from .pick import Row, choose
from .start import cmd_start
from .state import CLEAN_PREFIX, Repo, Session, named, short
from .ui import interactive, uninterrupted


def remote_of(branch: str) -> str | None:
    """The remote a local branch tracks, or None. "." is a local upstream."""
    found = git("config", f"branch.{short(branch)}.remote", check=False)
    return found if found and found != "." else None


def remotes_to_fetch(source: Session, head: str) -> list[str] | None:
    """The working line's remote, and the old base's. None means all of them:
    with no remote to go on, it is not obvious which one has the new line."""
    first = remote_of(source.working)
    if first is None:
        return None
    wanted = [first]
    for name in names_for(source.base, source, head):
        branch = name.split("~")[0].split("^")[0]
        if rev(f"refs/heads/{branch}") is not None:
            r = remote_of(branch)
        else:
            r = next((x for x in remotes() if branch.startswith(f"{x}/")), None)
        if r and r not in wanted:
            wanted.append(r)
    return wanted


def remotes() -> list[str]:
    # Longest first, so `bottle/rebase` is found before a remote called `bottle`.
    return sorted(git("remote").split(), key=len, reverse=True)


def fetch(which: list[str] | None) -> None:
    args = ["fetch", "--all"] if which is None else ["fetch", "--multiple", *which]
    sys.stdout.flush()
    code = subprocess.run(["git", *args], env=environment()).returncode
    if code != 0:
        # Offline is no reason not to use what is already here.
        print(f"curate: `git {' '.join(args)}` failed; carrying on with what "
              f"was already fetched.", file=sys.stderr)


def branch_name(ref: str) -> str:
    """A ref as the branch it is, without refs/heads/ or the remote's name."""
    if ref.startswith("refs/heads/"):
        return ref[len("refs/heads/"):]
    rest = ref[len("refs/remotes/"):]
    remote = next((r for r in remotes() if rest.startswith(f"{r}/")), None)
    return rest[len(remote) + 1:] if remote else rest


def branches(prefix: str, skip: set[str]) -> list[Row]:
    """The branches under `prefix`, newest first, as rows to choose from."""
    # Unstripped: str.strip() counts \x1f as whitespace, and would eat an
    # empty field at either end of the output.
    out = run(["for-each-ref", "--sort=-committerdate",
               "--format=%(symref)%1f%(refname)%1f%(refname:short)%1f"
               "%(committerdate:relative)%1f%(subject)", prefix]).stdout
    rows = []
    for line in out.splitlines():
        symref, ref, name, when, subject = line.split("\x1f")
        if symref or ref in skip:
            continue
        rows.append(Row([name, when, subject], value=ref))
    return rows


def choose_line(source: Session, fetched: list[str] | None) -> str:
    up = git("rev-parse", "--symbolic-full-name",
             f"{short(source.working)}@{{upstream}}", check=False)
    skip = {source.working, up} | source.repo.clean_lines()
    if fetched is None:
        rows = branches("refs/remotes/", skip)
    else:
        rows = [r for remote in fetched[:1]
                for r in branches(f"refs/remotes/{remote}/", skip)]
    rows.append(Row(["<show local>"],
                    expand=lambda: branches("refs/heads/", skip) or
                    [Row(["(no other local branches)"])]))
    header = [f"Transfer the review of {named(source.working)} to:", ""]
    return choose(header, rows, cursor=0)


def local_for(ref: str) -> tuple[str, str]:
    """The local branch for a chosen ref, and the commit it should be at.

    Makes nothing: what it says is only done once every question is answered.
    """
    if ref.startswith("refs/heads/"):
        return ref, ref
    if not any(ref.startswith(f"refs/remotes/{r}/") for r in remotes()):
        raise Refused(f"{ref} is not on any remote")
    local = f"refs/heads/{branch_name(ref)}"
    mine = rev(local)
    if mine is None or git_ok("merge-base", "--is-ancestor", local, ref):
        return local, ref                         # created, or fast-forwarded
    if git_ok("merge-base", "--is-ancestor", ref, local):
        return local, local                       # yours is ahead: keep it
    raise Refused(f"{named(local)} and {named(ref)} "
                  f"have diverged. Sort that out first, then `curate transfer "
                  f"{short(local)}`.")


def git_ok(*args: str) -> bool:
    return run(list(args)).returncode == 0


def cmd_transfer(repo: Repo, argv: list[str]) -> int:
    if len(argv) > 1:
        raise Refused("usage: curate transfer [<branch>]")
    repo.require_consistent()
    source = repo.session()
    if source is None:
        raise Refused("no review here to transfer. `curate list` shows them.")
    if source.fixup_pending:
        raise Refused("a fixup is paused on a conflict. Finish it or abort it "
                      "before transferring the review.")

    which = remotes_to_fetch(source, source.working)
    fetch(which)
    if argv:
        chosen = (f"refs/heads/{argv[0]}" if rev(f"refs/heads/{argv[0]}") else
                  git("rev-parse", "--symbolic-full-name", argv[0], check=False))
        if not chosen.startswith(("refs/heads/", "refs/remotes/")):
            raise Refused(f"no branch {argv[0]}")
    elif interactive():
        chosen = choose_line(source, which)
    else:
        raise Refused("which branch? `curate transfer <branch>`")

    local, tip = local_for(chosen)
    if local == source.working:
        raise Refused(f"that is {named(local)}, the branch under review already")
    if local in repo.clean_lines():
        raise Refused(f"{named(local)} is the clean line of a review")
    if any(s.working == local for s in repo.sessions()):
        raise Refused(f"{named(local)} has a review already: "
                      f"`git switch {short(local)} && curate review`")
    if rev(f"refs/heads/{CLEAN_PREFIX}/{short(local)}") is not None:
        raise Refused(f"branch {CLEAN_PREFIX}/{short(local)} already exists")
    base = choose_base(source, tip)

    # Every question is answered. From here on, things change, and stopping
    # between the switch and the copy would leave you on the new branch with
    # no review to transfer again -- so nothing stops it.
    with uninterrupted():
        mode = repo.mode
        if mode == "review":
            switch_mode(repo, "write")
        if rev(local) is None:
            git("branch", "--track", short(local), chosen)
        elif tip != local:
            git("update-ref", local, tip)             # fast-forward, checked above
        switched = run(["switch", short(local)])
        if switched.returncode != 0:
            if mode == "review":
                switch_mode(repo, "review")
            raise Refused(f"git switch {short(local)} failed: "
                          f"{switched.stderr.strip()}")
        repo.leave()                              # HEAD has left the review
        if cmd_start(repo, ["--copy", short(source.working), "--from", base]) != 0:
            return 1
        if mode == "review":
            switch_mode(repo, "review")
        return 0
