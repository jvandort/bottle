"""`curate integrate`: a pull request without the pull request.

What ships is the clean line if you curated one, and the working line if the
history came from elsewhere -- someone else's squash, or your own. It ships
only when everything is reviewed and committed: an uncommitted change is one
whoever writes the working line has not seen. Commits by others are adopted,
rewritten as yours, or kept as theirs, a decision made per author. The
destination is a new branch, or one that can be fast-forwarded, and never
anything else.

Every question comes before anything changes. Then the commits are rewritten
in the object database, the destination moved and switched to -- your files
already match it, so nothing on disk moves -- and the review is over: dropped,
and both its lines deleted. Their old tips are printed, so nothing is out of
reach.
"""

import shutil
import tempfile
from dataclasses import dataclass

from .carry import names_for
from .errors import Refused
from .gitcmd import commit_tree, git, metadata, parents_of, rev, run, tree_of
from .modes import switch_mode
from .pick import Row, choose, choose_many
from .state import CLEAN_PREFIX, Repo, Session, named, short
from .ui import interactive


@dataclass
class Author:
    name: str
    email: str
    commits: list[str]               # "a1b2c3d4  subject", newest first

    def __str__(self) -> str:
        return f"{self.name} <{self.email}>"


def me() -> tuple[str, str]:
    ident = git("var", "GIT_AUTHOR_IDENT")
    name, _, rest = ident.partition(" <")
    return name, rest.partition(">")[0]


def files_tree(session: Session) -> str:
    """Your files as a tree, untracked ones included, whatever the mode."""
    with tempfile.TemporaryDirectory() as d:
        idx = f"{d}/index"
        git("read-tree", session.working, index=idx)
        top = git("rev-parse", "--show-toplevel")
        git("-C", top, "add", "-A", index=idx)
        return git("write-tree", index=idx)


def changed(a: str, b: str) -> list[str]:
    return git("diff", "--name-only", "--no-renames", a, b).splitlines()


def listed(paths: list[str], most: int = 10) -> str:
    shown = "\n".join(f"  {p}" for p in paths[:most])
    more = f"\n  ... and {len(paths) - most} more" if len(paths) > most else ""
    return shown + more


# -- what ships ------------------------------------------------------------

def line_to_ship(repo: Repo, session: Session) -> str:
    """The clean line if you curated one, the working line otherwise; refused
    unless everything is reviewed, and committed on the working line."""
    working = rev(session.working)
    clean = rev(session.clean)
    if working is None or clean is None:
        raise Refused("a line of this review is missing. `curate status` says which.")
    files = files_tree(session)
    uncommitted = changed(f"{working}^{{tree}}", files)
    if uncommitted:
        raise Refused(
            f"your files have changes not committed on {named(session.working)}:\n"
            f"{listed(uncommitted)}\n"
            f"Whoever writes the working line has not seen them. Commit them "
            f"there, or hand them back, before integrating.")
    approved = session.approved_tree(repo.mode)
    unreviewed = changed(approved, files) if approved else []
    if unreviewed:
        raise Refused(f"{len(unreviewed)} file(s) still unreviewed:\n"
                      f"{listed(unreviewed)}\n`curate review` and finish first.")
    if clean != session.base:
        if tree_of(clean) != tree_of(working):
            raise Refused(
                f"the clean line {named(session.clean)} has commits on it, but "
                f"not all of what you approved. Commit the rest there first.")
        return clean
    if working == session.base:
        raise Refused("there is nothing to integrate: the working line is at the base")
    return working


# -- whose commits ---------------------------------------------------------

def others(commits: list[str]) -> list[Author]:
    mine = me()[1]
    found: dict[str, Author] = {}
    for c in commits:
        name, email, subject = git("log", "-1", "--format=%an%n%ae%n%s", c).split("\n")
        if email == mine:
            continue
        if email not in found:
            found[email] = Author(name, email, [])
        found[email].commits.append(f"{c[:8]}  {subject}")
    return list(found.values())


def which(authors: list[Author], most: int = 12) -> list[str]:
    """The commits, so you can see whether there is a squash to ask for
    before deciding anything. Under each author's name when there are several."""
    lines = []
    for a in authors:
        if len(authors) > 1:
            lines.append(f"  {a}")
        indent = "    " if len(authors) > 1 else "  "
        lines += [indent + c for c in a.commits]
    if len(lines) > most:
        lines = lines[:most - 1] + [f"  … and {len(lines) - most + 1} more"]
    return lines


def match(authors: list[Author], said: str) -> Author:
    hits = [a for a in authors if said in (a.name, a.email)]
    if not hits:
        raise Refused(f"none of these commits is by {said!r}. Their authors: "
                      + ", ".join(str(a) for a in authors))
    if len(hits) > 1:
        raise Refused(f"{said!r} could be any of "
                      + ", ".join(str(a) for a in hits) + "; use the email")
    return hits[0]


def to_adopt(authors: list[Author], adopt: list[str], keep: list[str]) -> set[str]:
    """The emails whose commits become yours. Every other author is decided
    on, by flags that name each one, or on a terminal, one by one."""
    if not authors:
        return set()
    if adopt or keep:
        yes = {match(authors, a).email for a in adopt}
        no = {match(authors, a).email for a in keep}
        if yes & no:
            raise Refused("an author cannot be both adopted and not")
        missing = [a for a in authors if a.email not in yes | no]
        if missing:
            raise Refused("every author has to be decided on. Not yet: "
                          + ", ".join(str(a) for a in missing)
                          + ". `--adopt <author>` or `--no-adopt <author>`.")
        return yes
    if not interactive():
        raise Refused("these commits are not yours: "
                      + ", ".join(f"{a} ({len(a.commits)})" for a in authors) + "\n"
                      + "\n".join(which(authors)) + "\n"
                      + "Say for each author: `--adopt <author>` or `--no-adopt <author>`.")
    total = sum(len(a.commits) for a in authors)
    if len(authors) == 1:
        a = authors[0]
        answer = choose([f"{total} commit(s) by {a} are not yours:",
                         *which(authors), ""],
                        [Row(["Adopt them as yours"], value="adopt"),
                         Row([f"Keep {a.name} as their author"], value="keep")],
                        cancelled="cancelled; nothing was changed")
        return {a.email} if answer == "adopt" else set()
    rows = [Row([str(a), f"{len(a.commits)} commit(s)"], value=a.email)
            for a in authors]
    return set(choose_many([f"{total} commits are not yours:", *which(authors), "",
                            "Tick the authors to adopt as yours; the rest stay "
                            "theirs.", ""], rows))


# -- where it goes ---------------------------------------------------------

def checked_out_elsewhere(branch: str) -> bool:
    out = git("worktree", "list", "--porcelain")
    return f"branch {branch}" in out.splitlines()


def can_take(branch: str, tip: str) -> bool:
    """A branch that `tip` fast-forwards."""
    return run(["merge-base", "--is-ancestor", branch, tip]).returncode == 0


def destination(session: Session, tip: str, into: str | None) -> str:
    """refs/heads/<name>: an existing branch to fast-forward, or a new one."""
    ours = {session.working, session.clean}

    def check(name: str) -> tuple[str | None, str]:
        ref = f"refs/heads/{name}"
        if run(["check-ref-format", "--branch", name]).returncode != 0:
            return None, f"{name!r} is not a branch name"
        if ref in ours or name.startswith(f"{CLEAN_PREFIX}/"):
            return None, f"{name} belongs to the review"
        if rev(ref) is not None and not can_take(ref, tip):
            return None, f"{name} cannot be fast-forwarded to this"
        if checked_out_elsewhere(ref):
            return None, f"{name} is checked out in another worktree"
        return ref, ""

    if into:
        ref, why = check(into)
        if ref is None:
            raise Refused(why)
        return ref
    if not interactive():
        raise Refused("into which branch? `--into <branch>`, new or one that "
                      "fast-forwards")
    guesses = {n.split("~")[0] for n in names_for(session.base, session, session.working)}
    rows = [Row(["<new branch…>"], prompt="new branch: ")]
    out = run(["for-each-ref", "--sort=-committerdate",
               "--format=%(refname)%1f%(committerdate:relative)%1f%(subject)",
               "refs/heads/"]).stdout
    for line in out.splitlines():
        ref, when, subject = line.split("\x1f")
        name = short(ref)
        if ref in ours or name.startswith(f"{CLEAN_PREFIX}/") or not can_take(ref, tip):
            continue
        rows.append(Row([name, when, subject], value=ref,
                        tag="best guess" if name in guesses else ""))

    def typed(text: str) -> tuple[str | None, str]:
        return check(text.strip())

    return choose([f"Integrate {named(session.working)} into:", ""], rows,
                  typed=typed, cancelled="cancelled; nothing was changed")


# -- doing it --------------------------------------------------------------

def rewrite(commits: list[str], adopt: set[str]) -> dict[str, str]:
    """Adopted commits as yours, author date kept; anything above one rebuilt
    on its new parent. Below the first, nothing changes."""
    name, email = me()
    new: dict[str, str] = {}
    for c in commits:
        parents = parents_of(c)
        mapped = [new.get(p, p) for p in parents]
        env, message = metadata(c)
        adopted = env["GIT_AUTHOR_EMAIL"] in adopt
        if not adopted and mapped == parents:
            new[c] = c
            continue
        if adopted:
            env |= {"GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email}
        new[c] = commit_tree(git("rev-parse", f"{c}^{{tree}}"), mapped, message,
                             env=env, sign=True)
    return new


def finish_review(repo: Repo, session: Session) -> list[str]:
    """Drop the review and delete both its lines. What was deleted, and where
    each one was.

    A line another review still needs, or the branch you are on, is left.
    """
    others = {line for s in repo.sessions() if s.slug != session.slug
              for line in (s.clean, s.working)}
    gone = []
    for ref in (session.clean, session.working):
        tip = rev(ref)
        if tip is not None and ref not in others and ref != repo.head:
            git("branch", "-D", short(ref))
            gone.append(f"{named(ref)} (was {tip[:8]})")
    shutil.rmtree(session.dir, ignore_errors=True)
    run(["update-ref", "-d", session.ref])
    return gone


def earlier(repo: Repo, session: Session) -> list[Session]:
    """The reviews this one was copied from, newest first: earlier versions
    of the same work. The chain ends at one already dropped."""
    found: list[Session] = []
    slug = session.copied_from
    while slug and slug != session.slug and all(s.slug != slug for s in found):
        prior = Session(repo, slug)
        if not prior.exists:
            break
        found.append(prior)
        slug = prior.copied_from
    return found


def cmd_integrate(repo: Repo, argv: list[str]) -> int:
    into: str | None = None
    adopt: list[str] = []
    keep: list[str] = []
    i = 0
    while i < len(argv):
        flag = argv[i]
        value = argv[i + 1] if i + 1 < len(argv) else None
        if flag in ("--into", "--adopt", "--no-adopt") and value is None:
            raise Refused(f"{flag} needs a value")
        if flag == "--into":
            into = value
        elif flag == "--adopt":
            adopt.append(value or "")
        elif flag == "--no-adopt":
            keep.append(value or "")
        else:
            raise Refused(f"unexpected argument: {flag}")
        i += 2

    repo.recover()
    repo.require_consistent()
    session = repo.session()
    if session is None:
        raise Refused("no review here to integrate. `curate list` shows them.")
    if session.fixup_pending:
        raise Refused("a fixup is paused on a conflict. Finish it or abort it first.")

    tip = line_to_ship(repo, session)
    shipping = git("rev-list", tip, f"^{session.base}").split()
    adopted = to_adopt(others(shipping), adopt, keep)
    target = destination(session, tip, into)
    # What a branch being fast-forwarded already has stays exactly as it is.
    commits = git("rev-list", "--reverse", "--topo-order", tip, f"^{session.base}",
                  *([f"^{target}"] if rev(target) else [])).split()

    # Every question is answered. From here on, things change.
    new_tip = rewrite(commits, adopted).get(tip, tip) if commits else tip
    old_target = rev(target)
    if repo.mode == "review":
        switch_mode(repo, "write")
    git("update-ref", "-m", "curate integrate", target, new_tip,
        *([old_target] if old_target else []))
    switched = run(["switch", short(target)])
    if switched.returncode != 0:
        raise Refused(f"{named(target)} is at {new_tip[:8]}, but switching to it "
                      f"failed: {switched.stderr.strip()}\nThe review is still here.")
    repo.recover()
    # Read before the review goes: what it was copied from is in its state.
    priors = earlier(repo, session)
    gone = finish_review(repo, session)
    ended = [(prior.working, finish_review(repo, prior)) for prior in priors]

    how = (f"fast-forwarded from {old_target[:8]}" if old_target
           else "a new branch")
    print(f"curate: integrated {len(commits)} commit(s) into {named(target)}, {how}.")
    if adopted:
        count = sum(1 for c in commits
                    if git("log", "-1", "--format=%ae", c) in adopted)
        print(f"        adopted {count} of them as yours.")
    if gone:
        print(f"        the review is over; deleted {', and '.join(gone)}.")
    for working, deleted in ended:
        what = f"; deleted {', and '.join(deleted)}" if deleted else ""
        print(f"        so is the review of {named(working)} it was copied "
              f"from{what}.")
    return 0
