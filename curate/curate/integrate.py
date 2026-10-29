"""`curate integrate`: a pull request without the pull request.

What ships is the clean line if you curated one, and the working line if the
history came from elsewhere -- someone else's squash, or your own. It ships
only when everything is reviewed and committed: an uncommitted change is one
whoever writes the working line has not seen. Commits by others are adopted,
rewritten as yours, or kept as theirs, a decision made per author. The
destination is a new branch, or one that can be fast-forwarded, and never
anything else -- but for one that has moved on since the review's base: with
your say-so, the commits are replayed onto it, as a rebase would, and it's
fast-forwarded to them. Your approvals hold, since each commit changes what it
did; the combination is untested, which it says. A conflict changes nothing.

Every question comes before anything changes. Then the commits are rewritten
in the object database, the destination moved and switched to -- your files
already match it, so nothing on disk moves, unless it was replayed onto -- and
the review is over: dropped,
and both its lines deleted. Their old tips are printed, so nothing is out of
reach.
"""

import tempfile
from dataclasses import dataclass

from .carry import names_for
from .errors import Refused
from .gitcmd import commit_tree, git, merge_tree, metadata, run
from .modes import switch_mode
from .pick import Row, choose, choose_many
from .state import Repo, Session, named, short
from .ui import interactive, uninterrupted


@dataclass
class Commit:
    oid: str
    tree: str
    parents: list[str]
    email: str                       # the author's


def commits_in(*revs: str) -> list[Commit]:
    """The commits `revs` name, oldest first: what `rewrite` needs of each,
    in one git rather than three a commit."""
    out = git("log", "--no-show-signature", "--reverse", "--topo-order",
              "--format=%H%x1f%T%x1f%P%x1f%ae", *revs)
    commits = []
    for line in out.splitlines():
        oid, tree, parents, email = line.split("\x1f")
        commits.append(Commit(oid, tree, parents.split(), email))
    return commits


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
        session.repo.index_of(session.working, idx)
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
    working = repo.objects.rev(session.working)
    clean = repo.objects.rev(session.clean)
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
                      f"{listed(unreviewed)}\n`curate read` and finish first.")
    if clean != session.base:
        if repo.objects.tree(clean) != repo.objects.tree(working):
            raise Refused(
                f"the clean line {named(session.clean)} has commits on it, but "
                f"not all of what you approved. Commit the rest there first.")
        return clean
    if working == session.base:
        raise Refused("there is nothing to integrate: the working line is at the base")
    return working


# -- whose commits ---------------------------------------------------------

def others(*revs: str) -> list[Author]:
    """Who wrote the commits `revs` name, besides you, newest first."""
    mine = me()[1]
    found: dict[str, Author] = {}
    out = git("log", "--no-show-signature", "--format=%H%x1f%an%x1f%ae%x1f%s", *revs)
    for line in out.splitlines():
        c, name, email, subject = line.split("\x1f", 3)
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


def moved_on(session: Session, branch: str) -> bool:
    """A branch that has the review's base and has moved on since: one to replay the review onto."""
    return run(["merge-base", "--is-ancestor", session.base, branch]).returncode == 0


@dataclass
class Destination:
    ref: str
    replay: bool                     # moved on since the base: the commits are replayed onto it
    said_so: bool = False            # picked from a list that said it replays


def destination(session: Session, tip: str, into: str | None) -> Destination:
    """refs/heads/<name>: an existing branch to fast-forward, or to replay onto, or a new one.

    Never this review's working line, nor any review's clean line: what is on
    a clean line is what that review approved, and nothing else puts it there.
    Another review's working line is fine -- moving one is how its review gets
    more to read.
    """
    taken = session.repo.clean_lines() | {session.working}

    def check(name: str) -> tuple[str | None, str]:
        ref = f"refs/heads/{name}"
        if run(["check-ref-format", "--branch", name]).returncode != 0:
            return None, f"{name!r} is not a branch name"
        if ref in taken:
            return None, f"{name} belongs to a review"
        replay = session.repo.objects.rev(ref) is not None and not can_take(ref, tip)
        if replay and not moved_on(session, ref):
            return None, (f"{name} cannot be fast-forwarded to this, nor replayed onto: "
                          f"it does not have the review's base")
        if checked_out_elsewhere(ref):
            return None, f"{name} is checked out in another worktree"
        return ref, ""

    def found(ref: str, said_so: bool = False) -> Destination:
        replay = session.repo.objects.rev(ref) is not None and not can_take(ref, tip)
        return Destination(ref, replay, said_so and replay)

    if into:
        ref, why = check(into)
        if ref is None:
            raise Refused(why)
        return found(ref)
    if not interactive():
        raise Refused("into which branch? `--into <branch>`, new or one that "
                      "fast-forwards or has moved on since the base")
    guesses = {n.split("~")[0] for n in names_for(session.base, session, session.working)}
    rows = [Row(["<new branch…>"], prompt="new branch: ")]
    # --merged: the branches `tip` fast-forwards, all in one call. Asking
    # merge-base about each in turn costs a git per branch you have ever made.
    out = run(["for-each-ref", "--sort=-committerdate", f"--merged={tip}",
               "--format=%(refname)%1f%(committerdate:relative)%1f%(subject)",
               "refs/heads/"]).stdout
    for line in out.splitlines():
        ref, when, subject = line.split("\x1f")
        name = short(ref)
        if ref in taken:
            continue
        rows.append(Row([name, when, subject], value=ref,
                        tag="best guess" if name in guesses else ""))
    # Then those moved on since the base, to replay onto.
    out = run(["for-each-ref", "--sort=-committerdate", f"--contains={session.base}",
               f"--no-merged={tip}", "--format=%(refname)%1f%(committerdate:relative)%1f%(subject)",
               "refs/heads/"]).stdout
    for line in out.splitlines():
        ref, when, subject = line.split("\x1f")
        name = short(ref)
        if ref in taken or checked_out_elsewhere(ref):
            continue
        rows.append(Row([name, when, subject], value=ref,
                        tag="replays onto it" + (", best guess" if name in guesses else "")))

    def typed(text: str) -> tuple[str | None, str]:
        return check(text.strip())

    picked = choose([f"Integrate {named(session.working)} into:", ""], rows,
                    typed=typed, cancelled="cancelled; nothing was changed")
    return found(picked, said_so=any(row.value == picked and "replays" in row.tag for row in rows))


def agree_to_replay(target: Destination, base: str, said: bool) -> None:
    """Replaying needs your say-so: --replay, a list that said it replays, or a yes."""
    if said or target.said_so:
        return
    ahead = git("rev-list", "--count", f"{base}..{target.ref}")
    what = (f"{named(target.ref)} has moved on by {ahead} commit(s) since this review's base, "
            f"so it cannot be fast-forwarded to this")
    if not interactive():
        raise Refused(f"{what}. `--replay` replays the commits onto it, as a rebase "
                      f"would; the result is untested together.")
    choose([f"{what}.", "Replay the commits onto it, as a rebase would? Your approvals "
            "hold; the result is untested together.", ""],
           [Row(["Replay onto it"], value="yes")], cancelled="cancelled; nothing was changed")


# -- doing it --------------------------------------------------------------

def rewrite(commits: list[Commit], adopt: set[str]) -> dict[str, str]:
    """Adopted commits as yours, author date kept; anything above one rebuilt
    on its new parent. Below the first, nothing changes."""
    new: dict[str, str] = {}
    for c in commits:
        mapped = [new.get(p, p) for p in c.parents]
        new[c.oid] = c.oid if c.email not in adopt and mapped == c.parents else remade(c, c.tree, mapped, adopt)
    return new


def remade(c: Commit, tree: str, parents: list[str], adopt: set[str]) -> str:
    """`c` with this tree and these parents: the same message, author and author date -- you, if adopted."""
    env, message = metadata(c.oid)
    if c.email in adopt:
        name, email = me()
        env |= {"GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email}
    return commit_tree(tree, parents, message, env=env, sign=True)


def replay(commits: list[Commit], onto: str, adopt: set[str]) -> tuple[str, int, list[Commit]]:
    """The commits replayed onto `onto`, adopted as `rewrite` would: the new tip, how many
    there are, and those that had nothing left to do there (on it already), dropped.
    A merge commit, or a conflict, refuses: the commits made are only objects, unreferenced."""
    if any(len(c.parents) != 1 for c in commits):
        raise Refused("a merge commit cannot be replayed. Rebase the working line onto the "
                      "destination yourself, then `curate update-base`.")
    new: dict[str, str] = {}
    dropped: list[Commit] = []
    tip = onto
    for c in commits:
        parent = new.get(c.parents[0], onto)
        tree, conflicts = merge_tree(c.parents[0], parent, c.oid)
        if conflicts:
            raise Refused(f"replaying {c.oid[:8]} conflicts, in:\n{listed(sorted(conflicts))}\n"
                          f"Nothing was changed. Rebase the working line onto the destination "
                          f"yourself, then `curate update-base`.")
        if tree == git("rev-parse", f"{parent}^{{tree}}"):
            new[c.oid] = parent
            dropped.append(c)
        else:
            new[c.oid] = tip = remade(c, tree, [parent], adopt)
    return tip, len(commits) - len(dropped), dropped


def finish_review(repo: Repo, session: Session, keep: str) -> list[str]:
    """Drop the review and delete both its lines. What was deleted, and where
    each one was.

    A line another review still needs is left, and so is `keep`: the branch
    the work was integrated into, which an earlier version was reviewed on
    when the copy was stacked on top of it.
    """
    others = {line for s in repo.sessions() if s.slug != session.slug
              for line in (s.clean, s.working)}
    gone = []
    for ref in (session.clean, session.working):
        tip = repo.objects.rev(ref)
        if tip is not None and ref not in others and ref != keep:
            git("branch", "-D", short(ref))
            gone.append(f"{named(ref)} (was {tip[:8]})")
    repo.forget(session)
    return gone


def earlier(repo: Repo, session: Session) -> list[Session]:
    """The reviews this one was copied from, newest first: earlier versions
    of the same work. The chain ends at one already dropped -- by its id,
    since a review started since on the same branch is not an earlier one."""
    by_id = {s.id: s for s in repo.sessions()}
    found: list[Session] = []
    prior = by_id.get(session.copied_from)
    while (prior is not None and prior.slug != session.slug
           and all(s.slug != prior.slug for s in found)):
        found.append(prior)
        prior = by_id.get(prior.copied_from)
    return found


def cmd_integrate(repo: Repo, argv: list[str]) -> int:
    into: str | None = None
    adopt: list[str] = []
    keep: list[str] = []
    said_replay = False
    i = 0
    while i < len(argv):
        flag = argv[i]
        if flag == "--replay":
            said_replay = True
            i += 1
            continue
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

    session = repo.session()
    if session is None:
        raise Refused("no review here to integrate. `curate list` shows them.")
    if session.fixup_pending:
        raise Refused("a fixup is paused on a conflict. Finish it or abort it first.")

    tip = line_to_ship(repo, session)
    adopted = to_adopt(others(tip, f"^{session.base}"), adopt, keep)
    chosen = destination(session, tip, into)
    target = chosen.ref
    old_target = repo.objects.rev(target)
    if chosen.replay:
        agree_to_replay(chosen, session.base, said_replay)
    # What a branch being fast-forwarded already has stays exactly as it is.
    commits = commits_in(tip, f"^{session.base}", *([f"^{target}"] if old_target else []))
    dropped: list[Commit] = []
    count = len(commits)
    if chosen.replay and old_target:
        # Every question is answered; a conflict refuses here, having made only objects.
        new_tip, count, dropped = replay(commits, old_target, adopted)

    # From here on, things change, and stopping half way would leave a
    # destination moved and a review that refuses to integrate again -- so
    # nothing stops it.
    with uninterrupted():
        if not chosen.replay:
            new_tip = rewrite(commits, adopted).get(tip, tip) if commits else tip
        if repo.mode == "review":
            switch_mode(repo, "write")
        if chosen.replay:
            # Your files are the shipped line's: they become the destination's, with what it has since.
            git("read-tree", "-m", "-u", tip, new_tip)
        git("update-ref", "-m", "curate integrate", target, new_tip,
            *([old_target] if old_target else []))
        # Not a checkout: your files already match the destination, so this is
        # HEAD and the index, as a mode switch is. The reflog says what the
        # `git switch` this replaces said.
        git("symbolic-ref", "-m", f"checkout: moving from "
            f"{short(session.working)} to {short(target)}", "HEAD", target)
        git("reset", "-q")
        # Read before the review goes: what it was copied from is in its state.
        priors = earlier(repo, session)
        gone = finish_review(repo, session, keep=target)
        ended = [(prior.working, finish_review(repo, prior, keep=target))
                 for prior in priors]

    how = (f"replayed onto it at {old_target[:8]}" if chosen.replay and old_target
           else f"fast-forwarded from {old_target[:8]}" if old_target
           else "a new branch")
    print(f"curate: integrated {count} commit(s) into {named(target)}, {how}.")
    if adopted:
        mine = sum(1 for c in commits if c.email in adopted and c not in dropped)
        print(f"        adopted {mine} of them as yours.")
    if dropped:
        print(f"        {len(dropped)} more had nothing left to do there, so were dropped: "
              + ", ".join(c.oid[:8] for c in dropped) + ".")
    if chosen.replay:
        print("        what it makes together is untested: run your tests.")
    if gone:
        print(f"        the review is over; deleted {', and '.join(gone)}.")
    for working, deleted in ended:
        what = f"; deleted {', and '.join(deleted)}" if deleted else ""
        print(f"        so is the review of {named(working)} it was copied "
              f"from{what}.")
    return 0
