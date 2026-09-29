"""What a review is, where it is kept, and the invariant that protects it.

State that matters goes in a ref: the approved set is `refs/curate/<clean>`, a
real commit with a reflog. The index files under `.git/curate` are a cache, and
can be deleted freely.
"""

import os
import sys
from pathlib import Path

from .errors import Refused
from .gitcmd import commit_tree, git, rev, run, tree_of

VERSION = "0.1"                  # the tool version

# The layout on disk under .git/curate.
#
# Bump it when a curate that predates your change would misread what it finds:
# a file renamed or moved, a format altered, a meaning changed. Do not bump for
# something purely additive that an older curate ignores harmlessly -- a bump
# costs every user of an older curate an error, so spend it on real breakage.
#
# What happens then is handled here and nowhere else, because this is the only
# place the number is read:
#
#   a HIGHER number than we know  -> refuse. Half-understanding state a newer
#                                    curate wrote is how approvals get lost.
#   a LOWER number                -> migrate, by deleting the index files and
#                                    letting them rebuild from the ref. They
#                                    are a cache, so that is a complete
#                                    migration for any change confined to them.
#
# If you ever change something the caches do not cover -- the session layout,
# the shape of a paused fixup -- deleting them is no longer enough, and the
# bump has to arrive with code in `migrate` that understands the older shape.
STATE_VERSION = 1
CLEAN_PREFIX = "curate"          # clean lines live in their own namespace


def read(path: Path) -> str:
    try:
        return path.read_text().strip()
    except OSError:
        return ""


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text if text.endswith("\n") else text + "\n")


def encode(name: str) -> str:
    """A branch name as one ref path segment.

    refs/curate/agent/foo and refs/curate/agent cannot both exist, and both
    branch names are legal, so the separator has to go.
    """
    return name.replace("%", "%25").replace("/", "%2F")


def short(ref: str) -> str:
    return ref[len("refs/heads/"):] if ref.startswith("refs/heads/") else ref


def named(ref: str | None) -> str:
    """A branch name in running prose, quoted.

    Without this, "HEAD is elsewhere" reads as a sentence about vagueness
    rather than a sentence about a branch called `elsewhere`.
    """
    return f"'{short(ref)}'" if ref else "a detached HEAD"


class Session:
    """A (working line, clean line, review index) triple, keyed by clean branch."""

    def __init__(self, repo: "Repo", slug: str) -> None:
        self.repo = repo
        self.slug = slug
        self.dir = repo.state / "sessions" / slug
        self.clean = read(self.dir / "clean")
        self.working = read(self.dir / "working")
        self.base = read(self.dir / "base")

    @property
    def exists(self) -> bool:
        return self.dir.is_dir()

    @property
    def ref(self) -> str:
        return f"refs/curate/{self.slug}"

    def index(self, mode: str) -> Path:
        return self.dir / f"index.{mode}"

    @property
    def fixup_dir(self) -> Path:
        return self.dir / "fixup"

    @property
    def fixup_pending(self) -> bool:
        return self.fixup_dir.is_dir()

    # -- the approved set ---------------------------------------------------

    def approved_index(self, mode: str) -> Path | None:
        """Whichever file currently holds the approved set, or None if it is
        only in the ref. Index files are a cache; the ref is the record."""
        if mode == "review":
            return self.repo.gitdir / "index"
        parked = self.index("review")
        return parked if parked.exists() else None

    def approved_tree(self, mode: str) -> str | None:
        idx = self.approved_index(mode)
        if idx is not None:
            p = run(["write-tree"], index=idx)
            if p.returncode == 0:
                return p.stdout.strip()
        return tree_of(self.ref) or tree_of(self.clean)

    def record(self, mode: str) -> None:
        """Put the approved set in a ref, so an index file can be lost freely.

        Only when it has actually changed: rewriting it with an identical tree
        would move a ref for no reason, and callers watch for that.
        """
        tree = self.approved_tree(mode)
        if tree is None or tree_of(self.ref) == tree:
            return
        tip = rev(self.clean)
        parents = [tip] if tip else []
        git("update-ref", self.ref, commit_tree(tree, parents, "approved"))

    def rebuild(self, mode: str) -> None:
        """Rebuild a lost index file from the ref. Costs one re-stat."""
        path = self.index(mode)
        source = (self.ref if mode == "review" and rev(self.ref) else
                  self.clean if mode == "review" else self.working)
        tmp = path.with_suffix(".rebuilding")
        tmp.unlink(missing_ok=True)
        git("read-tree", source, index=tmp)
        run(["update-index", "--refresh", "-q"], index=tmp)   # non-zero is normal
        tmp.replace(path)


class Repo:
    def __init__(self) -> None:
        if "GIT_DIR" in os.environ:                  # hooks hand us a relative one
            os.environ["GIT_DIR"] = os.path.abspath(os.environ["GIT_DIR"])
        found = run(["rev-parse", "--absolute-git-dir"])
        if found.returncode != 0:
            raise Refused("not a git repository")
        self.gitdir = Path(found.stdout.strip())
        self.state = self.gitdir / "curate"

    def stamp(self) -> None:
        write(self.state / "version", str(STATE_VERSION))

    def check_state_version(self) -> None:
        """Refuse a newer layout, migrate an older one. See STATE_VERSION."""
        if not self.state.is_dir():
            return
        found = read(self.state / "version")
        if not found or not found.isdigit():
            # Every curate that writes state stamps it, from the moment the
            # directory is created. Anything else is malformed, not old, and
            # there is nothing to be lenient about.
            saw = "is missing" if not found else f"reads {found!r}"
            raise Refused(
                f"this repository's curate state is malformed: "
                f".git/curate/version {saw}.\n"
                f"Nothing under .git/curate is durable -- your approvals are in "
                f"refs/curate/*, and both lines are branches -- so removing that "
                f"directory starts the review again and loses only the caches.")
        if int(found) > STATE_VERSION:
            raise Refused(
                f"this repository's curate state is version {found}, and this "
                f"curate (v{VERSION}) understands {STATE_VERSION}.\n"
                f"Upgrade curate, or `curate drop <branch>` to start over.")
        if int(found) < STATE_VERSION:
            self.migrate(int(found))

    def migrate(self, found: int) -> None:
        """Bring an older layout forward by throwing the caches away.

        The approved set is in a ref and the working line is in git; the index
        files are the only thing here that is derived, so rebuilding them is a
        complete migration for anything that only touched them. It costs one
        re-stat, the same scan `git status` does.
        """
        for session in self.sessions():
            for mode in ("write", "review"):
                session.index(mode).unlink(missing_ok=True)
        write(self.state / "version", str(STATE_VERSION))
        print(f"curate: state upgraded from version {found} to {STATE_VERSION}; "
              f"the index caches will rebuild.", file=sys.stderr)

    # -- global state -------------------------------------------------------

    @property
    def mode(self) -> str:
        return read(self.state / "mode") or "none"

    @property
    def head(self) -> str:
        p = run(["symbolic-ref", "-q", "HEAD"])
        return p.stdout.strip() if p.returncode == 0 else ""

    def session(self) -> Session | None:
        slug = read(self.state / "active")
        if not slug:
            return None
        s = Session(self, slug)
        return s if s.exists else None

    def sessions(self) -> list[Session]:
        root = self.state / "sessions"
        if not root.is_dir():
            return []
        return [Session(self, d.name) for d in sorted(root.iterdir()) if d.is_dir()]

    def set_state(self, mode: str, session: Session | None) -> None:
        """Three writes, deliberately not made atomic.

        A crash between them leaves a mode and an active session that disagree
        with each other. Nothing believes them: every command compares the
        recorded mode against HEAD before touching anything, fails that
        comparison, and re-anchors on HEAD, which is the one thing here that
        git keeps honest. tests/test_guardrails.py TornWrite drives both halves.

        Considered and declined: one file written through a temporary and
        renamed. It would prevent nothing observable, because the check that
        makes a torn write survivable has to exist anyway -- git moves HEAD out
        from under a review, and no amount of atomicity here changes that. Worth
        revisiting the moment anything goes under .git/curate that cannot be
        re-derived from HEAD or rebuilt from a ref; until then there is nothing
        for it to protect.
        """
        write(self.state / "version", str(STATE_VERSION))
        write(self.state / "mode", mode)
        write(self.state / "active", session.slug if session else "")

    # -- the invariant ------------------------------------------------------

    def expected_head(self, session: Session | None = None,
                      mode: str | None = None) -> str | None:
        session = session or self.session()
        mode = mode or self.mode
        if session is None:
            return None
        return session.clean if mode == "review" else session.working

    def consistent(self) -> bool:
        """HEAD is the branch the recorded mode says it should be.

        The one check that converts silent loss of the approved set into an
        error. It costs one symbolic-ref.
        """
        expected = self.expected_head()
        return expected is None or self.head == expected

    def require_consistent(self) -> None:
        # Resolved once rather than re-read per line: the session that failed
        # the check has to be the one named in the message, and a session that
        # went away between the two reads would crash instead of explaining.
        session = self.session()
        expected = self.expected_head(session)
        if session is None or expected is None or self.head == expected:
            return
        raise Refused(
            f"{self.mode} mode expects HEAD to be {named(expected)}, but "
            f"HEAD is {named(self.head)}.\n"
            f"Something moved it out from under the review. Nothing has been "
            f"touched, and your approvals are safe in {session.ref}.\n"
            f"To get back:  git switch {short(session.working)} && curate review")

    def recover(self) -> None:
        """Re-anchor on HEAD after being moved out, if HEAD names a session.

        The checkout itself is survivable; renaming an index file blindly
        afterwards is not. So we work out which mode HEAD actually implies and
        throw away whatever index file that makes stale.
        """
        if self.consistent():
            return
        head = self.head
        for session in self.sessions():
            if head == session.clean:
                mode = "review"
            elif head == session.working:
                mode = "write"
            else:
                continue
            # The live index belongs to HEAD's branch now; the parked file for
            # that same mode is a leftover from before the yank.
            session.index(mode).unlink(missing_ok=True)
            if not session.index("review" if mode == "write" else "write").exists():
                session.rebuild("review" if mode == "write" else "write")
            self.set_state(mode, session)
            print(f"curate: recovered; HEAD is {named(head)}, so this is {mode} mode.",
                  file=sys.stderr)
            return


def review_complete() -> bool:
    """Whether nothing is left unreviewed, in review mode.

    Only meaningful there, and only there is it asked: the live index is the
    approved set, so the second status column -- what differs between the index
    and your files -- is exactly what you have not read. The review is finished
    when there is none of it.
    """
    p = run(["status", "--porcelain"])
    if p.returncode != 0:
        return False
    return not any(len(line) > 1 and line[1] != " " for line in p.stdout.splitlines())
