"""Following the working line's upstream while you read.

When a fetch moves the branch the working line tracks, and you are reading,
the working line comes along -- files and all -- so your editor shows what
just arrived as unread, without leaving read mode. The `reference-transaction`
hook calls this, whoever fetched.

Only ever a fast-forward, and only when nothing of yours is in the way: the
files move by a two-tree `read-tree -m -u` on an index of the working line's
own, which refuses to overwrite a change you made, and then the branch moves,
checked against where it was. HEAD and the live index -- the clean line and
your approvals -- are not touched, nor is a paused fixup. Write mode's parked
index was built for the old tip, so it goes; it rebuilds on the next switch.
"""

import sys

from .gitcmd import run
from .state import Repo, named, short

ZERO = "0" * 40


def say(message: str) -> None:
    print(f"curate: {message}", file=sys.stderr)


def follow(repo: Repo, moved: list[tuple[str, str, str]]) -> None:
    """`moved` is the transaction's (old, new, ref) lines."""
    session = repo.session()
    if session is None or repo.mode != "review" or not repo.consistent():
        return
    upstream = run(["rev-parse", "--symbolic-full-name", f"{short(session.working)}@{{upstream}}"]).stdout.strip()
    new = next((n for _, n, ref in moved if ref == upstream and n != ZERO), None)
    tip = repo.objects.rev(session.working)
    if not upstream or new is None or tip is None or new == tip:
        return
    if run(["merge-base", "--is-ancestor", tip, new]).returncode != 0:
        say(f"{short(upstream)} moved, but {named(session.working)} has commits it doesn't, so it "
            f"stayed where it was. `curate git pull` merges them.")
        return
    scratch = repo.state / "upstream.index"
    try:
        repo.index_of(tip, scratch)
        run(["update-index", "--refresh", "-q"], index=scratch)
        files = run(["read-tree", "-m", "-u", tip, new], index=scratch)
    finally:
        scratch.unlink(missing_ok=True)
    if files.returncode != 0:
        why = next((line.strip() for line in files.stderr.splitlines() if line.strip()), "read-tree refused")
        say(f"{short(upstream)} moved, but {named(session.working)} stayed where it was: {why}")
        return
    if run(["update-ref", session.working, new, tip]).returncode != 0:
        # The files moved and the branch didn't: something else moved it meanwhile.
        say(f"{named(session.working)} moved under curate while it followed {short(upstream)}; "
            f"your files are at {new[:8]}. `curate status` says where things stand.")
        return
    session.index("write").unlink(missing_ok=True)
    say(f"{named(session.working)} followed {short(upstream)} to {new[:8]}, files and all; "
        f"what's new is unread.")
