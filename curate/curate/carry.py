"""Copying a review onto another working line: the clean line and the approvals.

Whoever writes the working line may push a rebase as a new branch rather than
force-push it, and the review belongs with the new one. It is copied, never moved: the review it came from
is left exactly as it was, so a copy that goes wrong costs a `drop`.

On the same base that is two refs: the clean line's tip and the approved tree
are taken as they are, and whatever the rebase changed shows up as unreviewed.
On a new base the clean line is replayed onto it, entirely in the object
database, and the approvals merged across. Nothing here ever stops on a
conflict. A clean commit that conflicts ends the replay, and it and every
commit after it fall back to approved-but-uncommitted; an approval that
conflicts falls back to unreviewed. Both are still in the old review, so
falling back loses nothing.
"""

import tempfile
from dataclasses import dataclass, field

from .errors import Refused
from .gitcmd import (commit_tree, git, merge_tree, metadata, parents_of, rev,
                     run, tree_of)
from .state import CLEAN_PREFIX, Repo, Session, named, short
from .ui import default_base, interactive, pick_base

# Where a base is usually found, in the order a person would name it.
MAINLINES = ("origin/HEAD", "origin/main", "origin/master", "main", "master")


@dataclass
class Carried:
    tip: str                         # the new clean line
    approved: str                    # the approved tree, on top of it
    replayed: list[str] = field(default_factory=list)       # subjects
    upstream: list[str] = field(default_factory=list)       # already in the base
    uncommitted: list[str] = field(default_factory=list)    # replay stopped here
    unreviewed: list[str] = field(default_factory=list)     # paths


def find(repo: Repo, name: str) -> Session:
    for s in repo.sessions():
        if name in (short(s.clean), s.clean, short(s.working), s.working):
            if s.fixup_pending:
                raise Refused(f"the review of {named(s.working)} has a fixup "
                              f"paused on a conflict. Finish it or abort it "
                              f"before copying the review.")
            return s
    raise Refused(f"no review of {name} to copy. `curate list` shows them.")


def subject(commit: str) -> str:
    return git("log", "-1", "--format=%s", commit)


# -- the base --------------------------------------------------------------

def excluded(source: Session, head: str) -> list[str]:
    """Refs that would name the old base by the branches under review: true,
    and no help in finding the line it came from."""
    refs = [f"refs/heads/{CLEAN_PREFIX}/*", "refs/curate/*"]
    for line in (source.working, head):
        refs.append(line)
        up = git("rev-parse", "--symbolic-full-name", f"{short(line)}@{{upstream}}",
                 check=False)
        if up.startswith("refs/"):
            refs.append(up)
    return refs


def names_for(commit: str, source: Session, head: str) -> list[str]:
    """What a person would call this commit: the branches on it, and where it
    is on a mainline. Failing both, whatever name-rev finds."""
    skip = excluded(source, head)
    on = git("for-each-ref", "--points-at", commit, "--format=%(refname)",
             "refs/heads", "refs/remotes", "refs/tags").split()
    names = [git("rev-parse", "--abbrev-ref", r) for r in on
             if not any(_glob(p, r) for p in skip) and not r.endswith("/HEAD")]
    for line in MAINLINES:
        tip = rev(line)
        if tip and run(["merge-base", "--is-ancestor", commit, tip]).returncode == 0:
            name = git("rev-parse", "--abbrev-ref", line)
            if name not in names:
                ahead = git("rev-list", "--count", f"{commit}..{tip}")
                names.append(f"{name}~{ahead}")
            break
    if names:
        return names
    found = git("name-rev", "--name-only", *[f"--exclude={p}" for p in skip],
                commit, check=False)
    return [] if found in ("", "undefined") else [found.removeprefix("remotes/")]


def _glob(pattern: str, ref: str) -> bool:
    return ref.startswith(pattern[:-1]) if pattern.endswith("*") else ref == pattern


def guess(head: str, names: list[str]) -> tuple[str | None, str | None, str | None]:
    """Where the new line left the line the old base came from.

    Every candidate is a line the new one might have been rebased onto: the
    old base's branches, their upstreams -- the new line was rebased onto
    origin/main, while your main may not have moved since -- and the usual
    mainlines. The guess is the merge base nearest the new line's tip, so a
    stale branch still sitting on the old base cannot hide a rebase.
    """
    candidates: list[str] = []
    for name in names:
        branch = name.split("~")[0].split("^")[0]
        up = git("rev-parse", "--abbrev-ref", f"{branch}@{{upstream}}", check=False)
        candidates += [up, branch] if up else [branch]
    best: tuple[int, str, str, str] | None = None
    for via in [*candidates, *MAINLINES]:
        tip = rev(via) if via else None
        if tip is None:
            continue
        found = run(["merge-base", head, tip])
        if found.returncode != 0:
            continue
        base = found.stdout.strip()
        distance = int(git("rev-list", "--count", f"{base}..{head}"))
        if best is None or distance < best[0]:
            best = (distance, base, git("rev-parse", "--abbrev-ref", via), tip)
    if best is None:
        return default_base(head)
    return best[1], best[2], best[3]


def choose_base(source: Session, head: str) -> str:
    """The old base if nothing says otherwise; asked, if anything does."""
    names = names_for(source.base, source, head)
    best, via, tip = guess(head, names)
    if best == source.base:
        return source.base
    said = ", ".join(names) or "no branch"
    header = [f"Copying the review of {named(source.working)}",
              f"  old base  {source.base[:8]}  ({said})  {subject(source.base)}",
              ""]
    if not interactive():
        where = f"{best[:8]}" if best else "somewhere else"
        raise Refused(
            f"{named(head)} looks to be based on {where}, not on the old base "
            f"{source.base[:8]} ({said}). Pass --from <base> to say which.")
    return pick_base(head, best, via, tip, header=header,
                     marks={source.base: "old base"})


# -- the copy --------------------------------------------------------------

def approved_tree(repo: Repo, source: Session) -> str:
    active = repo.session()
    mode = repo.mode if active and active.slug == source.slug else "write"
    tree = source.approved_tree(mode)
    if tree is None:
        raise Refused(f"cannot read the approvals of {named(source.working)}")
    return tree


def carry(repo: Repo, source: Session, base: str) -> Carried:
    """The new clean line and approved tree. Writes objects, and no refs."""
    old_tip = rev(source.clean)
    if old_tip is None:
        raise Refused(f"{named(source.clean)} does not exist")
    approved = approved_tree(repo, source)
    if base == source.base:
        return Carried(tip=old_tip, approved=approved)

    commits = git("rev-list", "--reverse", "--first-parent",
                  f"{source.base}..{old_tip}").split()
    if any(len(parents_of(c)) > 1 for c in commits):
        raise Refused(f"{named(source.clean)} has a merge on it, and a merge "
                      f"cannot be replayed onto a new base")
    result = Carried(tip=base, approved="")
    done = source.base                   # the old commit the new tip stands for
    for i, c in enumerate(commits):
        tree, conflicts = merge_tree(base=parents_of(c)[0], ours=result.tip,
                                     theirs=c)
        if conflicts:
            result.uncommitted = [subject(x) for x in commits[i:]]
            break
        if tree == tree_of(result.tip):
            result.upstream.append(subject(c))
        else:
            env, message = metadata(c)
            result.tip = commit_tree(tree, [result.tip], message, env=env)
            result.replayed.append(subject(c))
        done = c

    # Everything approved since `done`, committed or not, onto the new tip.
    tree, conflicts = merge_tree(base=done, ours=result.tip,
                                 theirs=commit_tree(approved, [old_tip], "approved"))
    if conflicts:
        tree = ours_where_conflicted(tree, result.tip, list(conflicts))
        result.unreviewed = sorted(conflicts)
    result.approved = tree
    return result


def ours_where_conflicted(tree: str, tip: str, paths: list[str]) -> str:
    """The merged tree, with each conflicted path as the clean line has it:
    neither approved nor marked up, so it is simply unreviewed again."""
    with tempfile.TemporaryDirectory() as d:
        idx = f"{d}/index"
        git("read-tree", tree, index=idx)
        for path in paths:
            entry = git("ls-tree", tip, "--", path)
            if entry:
                mode, _, rest = entry.partition(" ")
                oid = rest.split()[1]
                git("update-index", "--cacheinfo", f"{mode},{oid},{path}", index=idx)
            else:
                git("update-index", "--force-remove", "--", path, index=idx)
        return git("write-tree", index=idx)


def report(source: Session, carried: Carried) -> None:
    print(f"curate: copied the review of {named(source.working)}.")
    if carried.replayed:
        print(f"        {len(carried.replayed)} clean commit(s) replayed onto the new base.")
    for s in carried.upstream:
        print(f"        dropped, already in the new base: {s}")
    if carried.uncommitted:
        print(f"        these conflicted on the new base and were not replayed; "
              f"what of them carried is approved but not committed:")
        for s in carried.uncommitted:
            print(f"          {s}")
    if carried.unreviewed:
        print(f"        approvals that did not carry, now unreviewed again:")
        for p in carried.unreviewed:
            print(f"          {p}")
