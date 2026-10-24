"""Choosing a branch to review, and getting onto it.

`start` and `transfer` both ask which branch, out of everything local and
everything just fetched, and both end up switching to one that may only exist
on a remote so far. Everything here but `switch_to` writes nothing, so a
question cancelled part way leaves the repository as it was.
"""

import subprocess
import sys

from .errors import Refused
from .gitcmd import environment, git, rev, run
from .pick import Row
from .state import Repo, named, short
from .timing import timed


def remote_of(branch: str) -> str | None:
    """The remote a local branch tracks, or None. "." is a local upstream."""
    found = git("config", f"branch.{short(branch)}.remote", check=False)
    return found if found and found != "." else None


def remotes() -> list[str]:
    # Longest first, so `bottle/rebase` is found before a remote called `bottle`.
    return sorted(git("remote").split(), key=len, reverse=True)


def fetch(which: list[str] | None) -> None:
    args = ["fetch", "--all"] if which is None else ["fetch", "--multiple", *which]
    sys.stdout.flush()
    with timed(["git", *args]):
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


def branches(*prefixes: str, skip: set[str]) -> list[Row]:
    """The branches under `prefixes`, newest first, as rows to choose from."""
    # Unstripped: str.strip() counts \x1f as whitespace, and would eat an
    # empty field at either end of the output.
    out = run(["for-each-ref", "--sort=-committerdate",
               "--format=%(symref)%1f%(refname)%1f%(refname:short)%1f"
               "%(committerdate:relative)%1f%(subject)", *prefixes]).stdout
    rows = []
    for line in out.splitlines():
        symref, ref, name, when, subject = line.split("\x1f")
        if symref or ref in skip:
            continue
        rows.append(Row([name, when, subject], value=ref))
    return rows


def to_review(repo: Repo) -> list[Row]:
    """Every branch there is to review, newest first, as rows to choose from.

    Local and remote together, because which one has the work is exactly what
    you do not want to have to know. A remote branch you already have locally
    is the local one, so it is listed once; the clean lines are not work to
    review, and are not listed at all.
    """
    skip = repo.clean_lines()
    rows = branches("refs/heads/", "refs/remotes/", skip=skip)
    local = {branch_name(r.value) for r in rows
             if r.value and r.value.startswith("refs/heads/")}
    # A clean line pushed somewhere is still a clean line.
    clean = {branch_name(c) for c in skip}
    reviewed = {s.working for s in repo.sessions()}
    head = repo.head
    kept = []
    for row in rows:
        assert row.value is not None
        if row.value.startswith("refs/remotes/"):
            if branch_name(row.value) in local | clean:
                continue
        if row.value == head:
            row.tag = "current"
        elif row.value in reviewed:
            row.tag = "has a review"
        kept.append(row)
    return kept


def local_for(ref: str, again: str | None = None) -> tuple[str, str]:
    """The local branch for a chosen ref, and the commit it should be at.
    `again`, if there is one, is the command that takes the branch, to run
    once a refusal is sorted out.

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
    then = f", then `{again} {short(local)}`" if again else ""
    raise Refused(f"{named(local)} and {named(ref)} have diverged. "
                  f"Sort that out first{then}.")


def git_ok(*args: str) -> bool:
    return run(list(args)).returncode == 0


def switch_to(local: str, chosen: str, tip: str) -> None:
    """Make `local` what `local_for` said, and check it out.

    Refused if git will not switch, with the branch put back as it was: made
    if it was not there, moved back if it was fast-forwarded.
    """
    before = rev(local)
    if before is None:
        git("branch", "--track", short(local), chosen)
    elif tip != local:
        git("update-ref", local, tip)             # fast-forward, checked by local_for
    switched = run(["switch", short(local)])
    if switched.returncode == 0:
        return
    if before is None:
        git("branch", "-D", short(local))
    elif tip != local:
        git("update-ref", local, before)
    raise Refused(f"git switch {short(local)} failed: {switched.stderr.strip()}")
