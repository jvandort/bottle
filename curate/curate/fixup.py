"""Folding approved changes into a clean commit that already exists.

All of it happens in the object database: no checkout, no stash, no temporary
worktree. The clean branch moves once, by a single update-ref, after every step
has succeeded.
"""

import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

from .errors import Refused
from .gitcmd import (Conflicts, blob, commit_tree, git, git_ok, merge_tree,
                     metadata, parents_of, rev, run, tree_of)
from .state import Repo, Session, named, read, short, write


@dataclass(frozen=True)
class FixupState:
    """A fixup paused on a conflict, as it sits on disk."""

    target: str              # the commit being folded into
    done: str                # the rebuilt clean tip so far, "" at the fold
    stage: str               # "fold" or "replay"
    current: str             # the commit being replayed, "" at the fold
    conflicted_tree: str     # merge-tree's tree, with markers in it
    todo: list[str]          # commits still to replay, oldest first


def fixup_state(session: Session) -> FixupState:
    d = session.fixup_dir
    return FixupState(
        target=read(d / "target"),
        done=read(d / "done"),
        stage=read(d / "stage"),
        current=read(d / "current"),
        conflicted_tree=read(d / "conflicted_tree"),
        todo=[l for l in read(d / "todo").splitlines() if l],
    )


# -- resolving -------------------------------------------------------------
#
# A conflict is resolved in files under `<fixup>/stages/`, not in your working
# tree, which is the whole reason you can keep writing code while one is
# paused. `materialise` puts them there, `scan` reads back what you did to
# them, and both `resolve` and `fixup --continue` go through the pair -- so
# editing them by hand works exactly as well as a merge tool does, which
# matters because a merge tool is not always configured.

SIDES = {1: "BASE", 2: "LOCAL", 3: "REMOTE"}


def conflicted(data: bytes) -> bool:
    """Whether a file still has conflict markers in it.

    The same test a person applies. There is no index here with unmerged
    entries to ask instead, since none of this touches one; and a resolution
    that deliberately keeps a line beginning `<<<<<<< ` is rare enough to be
    worth the false alarm, which `--continue` names the file for.
    """
    lines = data.split(b"\n")
    return (any(line.startswith(b"<<<<<<< ") for line in lines)
            and any(line.startswith(b">>>>>>> ") for line in lines))


def materialise(session: Session, conflicts: Conflicts) -> Path:
    """Write the three sides and the marked-up file, once.

    Once, because these files are where a resolution lives: a second `resolve`
    must not overwrite what you edited by hand the first time. All of it is
    bytes -- content is content, and a decode here would corrupt it.
    """
    stages = session.fixup_dir / "stages"
    if (session.fixup_dir / "materialised").exists():
        return stages
    tree = read(session.fixup_dir / "conflicted_tree")
    for path, sides in sorted(conflicts.items()):
        scratch = stages / path
        scratch.parent.mkdir(parents=True, exist_ok=True)
        for n, label in SIDES.items():
            # A missing stage means the file was added on one side only.
            content = blob(sides[n][1]) if n in sides else None
            scratch.with_name(scratch.name + "." + label).write_bytes(content or b"")
        # The conflicted tree already has ordinary markers in it, so it is the
        # right starting point whether or not a merge tool shows up.
        scratch.write_bytes(blob(f"{tree}:{path}") or b"")
    write(session.fixup_dir / "materialised", "1")
    return stages


def scan(session: Session,
         conflicts: Conflicts) -> tuple[list[str], list[str]]:
    """Read the stage files back: (resolutions, paths still conflicted).

    A resolution is `mode\toid\tpath`, with mode "0" for a file the
    resolution deletes. Called by `--continue` as well as by `resolve`, so a
    hand edit needs no second command to be noticed.

    What counts as resolved depends on what you were given. A marked-up file
    is resolved when the markers are gone. A file merge-tree could not mark up
    -- a binary one, where it writes one side and calls it a conflict -- has no
    markers to remove, so there the test is that you changed it at all.
    Otherwise walking away would silently pick a side.
    """
    stages = session.fixup_dir / "stages"
    if not (session.fixup_dir / "materialised").exists():
        return [], sorted(conflicts)
    tree = read(session.fixup_dir / "conflicted_tree")
    resolved, outstanding = [], []
    for path, sides in sorted(conflicts.items()):
        scratch = stages / path
        if not scratch.exists():           # deleting it is a resolution too
            resolved.append(f"0\t{'0' * 40}\t{path}")
            continue
        now = scratch.read_bytes()
        given = blob(f"{tree}:{path}") or b""
        done = not conflicted(now) if conflicted(given) else now != given
        if not done:
            outstanding.append(path)
            continue
        mode = sides.get(2, sides.get(3, ("100644", "")))[0]
        resolved.append(f"{mode}\t{git('hash-object', '-w', str(scratch))}\t{path}")
    return resolved, outstanding


def save_conflict(session: Session, stage: str, target: str, current: str,
                  done: str, todo: list[str], tree: str,
                  conflicts: Conflicts) -> None:
    d = session.fixup_dir
    d.mkdir(parents=True, exist_ok=True)
    write(d / "stage", stage)
    write(d / "target", target or "")
    write(d / "current", current or "")
    write(d / "done", done or "")
    write(d / "todo", "\n".join(todo))
    write(d / "conflicted_tree", tree)
    write(d / "conflicts", "\n".join(
        f"{n}\t{mode}\t{oid}\t{path}"
        for path, stages in conflicts.items()
        for n, (mode, oid) in sorted(stages.items())))
    # A fresh conflict, so any resolution of the previous one is gone with it.
    (d / "materialised").unlink(missing_ok=True)
    shutil.rmtree(d / "stages", ignore_errors=True)
    materialise(session, conflicts)


def read_stages(session: Session) -> Conflicts:
    out: Conflicts = {}
    for line in read(session.fixup_dir / "conflicts").splitlines():
        n, mode, oid, path = line.split("\t", 3)
        out.setdefault(path, {})[int(n)] = (mode, oid)
    return out


def report_conflict(repo: Repo, session: Session, conflicts: Conflicts,
                    step: int, total: int, into: str,
                    later_range: str | None) -> None:
    paths = sorted(conflicts)
    print(f'fixup: conflict folding into "{into}"  (step {step} of {total})')
    for path in paths:
        print(f"  {path}")
    for path in paths:
        if later_range:
            subjects = git("log", "--format=%s", later_range, "--", path).splitlines()
            for subject in subjects:
                # A fold conflict implies a replay conflict: the commit that
                # made this conflict will hit it again on its way back.
                print(f'\n  "{subject}" also changes {path}, so this will '
                      f"conflict again on replay.")
                print(f'  If the change belongs there, abort and fix up '
                      f'"{subject}" instead.')
    print("\n  resolve:  curate resolve   (or edit the marked-up files in")
    print(f"            {session.fixup_dir / 'stages'})")
    print("  then:     curate fixup --continue")


def replay(repo: Repo, session: Session, new: str,
           todo: list[str]) -> tuple[str | None, int]:
    """Replay the rest of the clean line onto a rebuilt commit."""
    while todo:
        y = todo[0]
        tree, conflicts = merge_tree(base=f"{y}^" if parents_of(y) else y,
                                     ours=y, theirs=new)
        if conflicts:
            save_conflict(session, "replay", read(session.fixup_dir / "target"),
                          y, new, todo, tree, conflicts)
            report_conflict(repo, session, conflicts,
                            step=2, total=2, into=git("log", "-1", "--format=%s", y),
                            later_range=None)
            return None, 2
        env, message = metadata(y)
        new = commit_tree(tree, [new], message, env=env)
        todo = todo[1:]
    return new, 0


def finish(repo: Repo, session: Session, new: str) -> int:
    git("update-ref", session.clean, new)
    shutil.rmtree(session.fixup_dir, ignore_errors=True)
    # Only matters when a conflict was resolved, since then the rebuilt tip's
    # tree is no longer the approved tree. Cheap, and always correct.
    git("reset", "--mixed", "-q", session.clean)
    session.record(repo.mode)
    print(f"fixup: {named(session.clean)} is now {new[:8]}")
    return 0


def cmd_fixup(repo: Repo, argv: list[str]) -> int:
    repo.recover()
    repo.require_consistent()
    session = repo.session()
    if session is None:
        raise Refused("no review here.")

    if "--abort" in argv:
        if not session.fixup_pending:
            raise Refused("no fixup in progress")
        # Nothing to undo: no ref moved and no file was written.
        shutil.rmtree(session.fixup_dir, ignore_errors=True)
        print("fixup: aborted. Nothing had been written, so nothing was undone.")
        return 0

    if repo.mode != "review":
        raise Refused("fixup folds in what you have approved, so it needs review "
                      "mode: `curate review`")

    if "--continue" in argv:
        if not session.fixup_pending:
            raise Refused("no fixup in progress")
        return continue_fixup(repo, session)

    if session.fixup_pending:
        raise Refused("a fixup is already paused. `curate fixup --continue` "
                      "or `curate fixup --abort`.")

    targets = [a for a in argv if not a.startswith("--")]
    if len(targets) != 1:
        raise Refused("usage: curate fixup <commit>")

    clean_tip = rev(session.clean)
    if clean_tip is None:
        raise Refused(f"the clean line {named(session.clean)} no longer exists. "
                      f"`curate drop {short(session.working)}` to forget the review.")
    target = rev(targets[0])
    if target is None:
        raise Refused(f"cannot resolve {targets[0]}")
    if not git_ok("merge-base", "--is-ancestor", target, clean_tip):
        raise Refused(f"{targets[0][:8]} is not on the clean line "
                      f"({named(session.clean)}); there is nothing to fold it into")
    if git("rev-list", "--min-parents=2", f"{target}..{clean_tip}"):
        raise Refused("the clean line is not linear between there and the tip; "
                      "a merge cannot be replayed")

    approved = git("write-tree")
    if approved == tree_of(clean_tip):
        raise Refused("nothing approved to fix up: the clean tip already has "
                      "exactly what you have staged")

    env, message = metadata(target)

    if target == clean_tip:               # amend: no replay needed
        new = commit_tree(approved, parents_of(target), message, env=env)
        return finish(repo, session, new)

    todo = git("rev-list", "--reverse", f"{target}..{clean_tip}").splitlines()
    fold = commit_tree(approved, [clean_tip], "fixup")

    tree, conflicts = merge_tree(base=clean_tip, ours=target, theirs=fold)
    if conflicts:
        save_conflict(session, "fold", target, "", "", todo, tree, conflicts)
        report_conflict(repo, session, conflicts, step=1, total=2,
                        into=message.splitlines()[0] if message else target[:8],
                        later_range=f"{target}..{clean_tip}")
        return 2
    new = commit_tree(tree, parents_of(target), message, env=env)
    rebuilt, code = replay(repo, session, new, todo)
    if code or rebuilt is None:
        return code
    return finish(repo, session, rebuilt)


def continue_fixup(repo: Repo, session: Session) -> int:
    state = fixup_state(session)
    conflicts = read_stages(session)
    # Re-read the stage files rather than trusting what `resolve` recorded: you
    # may have edited them since, or instead. This is also the check that keeps
    # conflict markers off the clean line, which is the one branch that exists
    # to not have any -- git rebase refuses here for the same reason.
    resolved, outstanding = scan(session, conflicts)
    if outstanding:
        raise Refused(
            "still conflicted, so there is nothing to continue with:\n  "
            + "\n  ".join(outstanding)
            + f"\nResolve them in {session.fixup_dir / 'stages'} -- `curate "
              f"resolve` opens your merge tool, or edit the marked-up files "
              f"there by hand.\n`curate fixup --abort` drops the fixup; "
              f"nothing has been written.")

    tmp = session.fixup_dir / "index"
    tmp.unlink(missing_ok=True)
    git("read-tree", state.conflicted_tree, index=tmp)
    for line in resolved:
        mode, oid, path = line.split("\t", 2)
        if mode == "0":
            git("update-index", "--force-remove", path, index=tmp)
        else:
            git("update-index", "--cacheinfo", f"{mode},{oid},{path}", index=tmp)
    tree = git("write-tree", index=tmp)
    tmp.unlink(missing_ok=True)

    if state.stage == "fold":
        env, message = metadata(state.target)
        new = commit_tree(tree, parents_of(state.target), message, env=env)
        todo = state.todo
    else:
        env, message = metadata(state.current)
        new = commit_tree(tree, [state.done], message, env=env)
        todo = state.todo[1:]

    shutil.rmtree(session.fixup_dir, ignore_errors=True)
    rebuilt, code = replay(repo, session, new, todo)
    if code or rebuilt is None:
        return code
    return finish(repo, session, rebuilt)


def cmd_resolve(repo: Repo, argv: list[str]) -> int:
    repo.recover()
    session = repo.session()
    if session is None or not session.fixup_pending:
        raise Refused("no conflict to resolve")

    conflicts = read_stages(session)
    stages = materialise(session, conflicts)
    tool = git("config", "merge.tool", check=False)
    cmd = git("config", f"mergetool.{tool}.cmd", check=False) if tool else ""

    for path in sorted(conflicts):
        scratch = stages / path
        if not cmd or not scratch.exists() or not conflicted(scratch.read_bytes()):
            continue                       # already dealt with; leave it alone
        env = {label: f"{scratch}.{label}" for label in SIDES.values()}
        env["MERGED"] = str(scratch)
        subprocess.run(["sh", "-c", cmd], env={**os.environ, **env})

    resolved, outstanding = scan(session, conflicts)
    if not cmd:
        print("resolve: no merge tool configured (`git config merge.tool`).")
        print(f"         The three sides, and the marked-up file, are in")
        print(f"         {stages}")
        print("         Edit the marked-up file there; `--continue` reads it back.")
    print(f"resolve: {len(resolved)} of {len(conflicts)} resolved"
          + (f"; still conflicted: {', '.join(outstanding)}" if outstanding else ""))
    print("then:    curate fixup --continue")
    return 0
