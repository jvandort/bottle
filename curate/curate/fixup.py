"""Folding approved changes into a clean commit that already exists.

All of it happens in the object database: no checkout, no stash, no temporary
worktree. The clean branch moves once, by a single update-ref, after every step
has succeeded.
"""

import os
import shutil
import subprocess
from dataclasses import dataclass

from .errors import Refused
from .gitcmd import (Conflicts, commit_tree, git, git_ok, merge_tree, metadata,
                     parents_of, rev, run, tree_of)
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
    (d / "resolved").unlink(missing_ok=True)


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
    print("\n  resolve:  curate resolve")
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
    tmp = session.fixup_dir / "index"
    tmp.unlink(missing_ok=True)
    git("read-tree", state.conflicted_tree, index=tmp)
    for line in read(session.fixup_dir / "resolved").splitlines():
        mode, oid, path = line.split("\t", 2)
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

    stages_dir = session.fixup_dir / "stages"
    stages_dir.mkdir(parents=True, exist_ok=True)
    conflicts = read_stages(session)
    tool = git("config", "merge.tool", check=False)
    cmd = git("config", f"mergetool.{tool}.cmd", check=False) if tool else ""
    resolved = []

    for path, stages in sorted(conflicts.items()):
        scratch = stages_dir / path
        scratch.parent.mkdir(parents=True, exist_ok=True)
        names = {1: "BASE", 2: "LOCAL", 3: "REMOTE"}
        for n, label in names.items():
            # A missing stage means the file was added on one side only.
            content = (git("cat-file", "blob", stages[n][1], check=False)
                       if n in stages else "")
            side = scratch.with_name(scratch.name + "." + label)
            side.write_text(content + "\n" if content else "")
        # The conflicted tree already has ordinary markers in it, so it is the
        # right starting point whether or not a merge tool shows up.
        merged = git("cat-file", "blob",
                     f"{read(session.fixup_dir / 'conflicted_tree')}:{path}",
                     check=False)
        scratch.write_text(merged + "\n" if merged else "")
        before = scratch.read_bytes()

        if not cmd:
            continue
        env = {"BASE": str(scratch) + ".BASE", "LOCAL": str(scratch) + ".LOCAL",
               "REMOTE": str(scratch) + ".REMOTE", "MERGED": str(scratch)}
        subprocess.run(["sh", "-c", cmd], env={**os.environ, **env})
        if scratch.read_bytes() != before:
            mode = stages.get(2, stages.get(3, ("100644", "")))[0]
            oid = git("hash-object", "-w", str(scratch))
            resolved.append(f"{mode}\t{oid}\t{path}")

    if resolved:
        write(session.fixup_dir / "resolved", "\n".join(resolved))
    paths = ", ".join(sorted(conflicts))
    if cmd:
        print(f"resolve: {len(resolved)} of {len(conflicts)} resolved ({paths})")
    else:
        print(f"resolve: no merge tool configured (`git config merge.tool`).")
        print(f"         The three sides, and the marked-up file, are in")
        print(f"         {stages_dir}: {paths}")
    print("then:    curate fixup --continue")
    return 0
