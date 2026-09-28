# Two lines

Status: design, mechanism tested. Not built.

**Not part of bottle.** This describes a standalone tool, and it lives in this
repo only until it has one of its own. Nothing in bottle should depend on it,
and it must not depend on bottle: the model below is for anyone writing code,
and an agent's branch is only one of the things it can point at.

## The idea

Most git workflows make you choose between writing freely and having a history
worth reading. You either commit as you go and clean up later with an
interactive rebase, or you hold everything uncommitted until you can compose it
properly -- which means a day's work living in a working tree, referenced by
nothing.

Keep two lines instead:

- **The working line.** Where you write. Messy commits, whatever order, rebased
  and squashed whenever you feel like it. Nobody reads it, including you.
- **The clean line.** What you ship. Every commit deliberate, composed out of
  changes you have looked at on purpose.

They are parallel, not sequential. You are never blocked from writing because
you have not curated, and never forced to curate because you want to commit.

## Two modes, one working tree

Git has three trees. Point them at the right things and both lines are just
`git status`:

| mode | HEAD | index | working tree | commit button |
| --- | --- | --- | --- | --- |
| **write** | working line | ordinary staging | your files | extends the working line |
| **review** | clean line | what you've approved | *the same files* | extends the clean line |

The working tree does not move when you switch. Only HEAD and the index do.
That is what makes the switch instant, safe and invisible to your editor's
index: no files change, so nothing reindexes and you do not lose your place.

What each mode shows falls out of the arrangement, and both are exactly what
you want to see:

- **Review mode** shows the clean line's tip against your files. Staged is what
  you have approved and not yet committed; unstaged is your review to-do list.
- **Write mode** shows the working line's tip against your files -- so only
  what you have touched since your last messy commit. The clean line is
  invisible, because it is not what you are doing.

Switching is three operations: save the current index file, restore the other
one, and point HEAD at the other branch. Nothing is recomputed; the stat cache
survives.

## Walkthrough

```
working line (ugly):   9c0551e more wip   1fec9f5 wip   abb87a4 start

REVIEW MODE -- approve one hunk of four
  approved:   -1 +feature-A
  commit  ->  clean line: 4ca414f "Add feature A"   abb87a4 start
  still unapproved:  -20 +feature-B  -30 +feature-C  -40 +debug-junk

worktree: 0 lines differ from the working line's tip  (untouched throughout)
```

`debug-junk` is the point. It sits in the working line forever and never gets
approved, so it never reaches the clean line. There is no stripping step and no
interactive rebase at the end -- it simply never gets in.

And because approval is **content**, not history, rebasing or squashing the
working line disturbs nothing. A rebase that changes no content leaves nothing
unapproved.

## Fixing up the clean line

Committing early to the clean line would be a trap if the clean line were
append-only: approve `feature-A`, commit it, then change `feature-A` in the
working line, and you are appending "fix feature A" to a history whose whole
point is not needing that.

It is not append-only. The clean line is yours and unpublished, so it can be
rewritten freely. Approve the new hunks and put them where they belong:

```sh
review fixup <clean-commit>
```

### It needs no working tree

The obvious implementation is `git commit --fixup` plus an autosquash rebase,
which is what many people already have as a shell alias. It does not fit here,
because a rebase checks out commits and the working tree is holding the working
line's content.

Stashing around it -- the usual trick -- is worse than it looks.
`git stash push --keep-index` **rewrites the files on disk** to the index state,
and `pop` rewrites them back: two full rewrites of every changed file in the
directory you are working in, two reindexes in your editor, and a failure
halfway leaves your real work in a stash. Handing the rebase a temporary
worktree instead avoids the mess but checks out the entire repository to do it.

Neither is necessary. Nothing here needs files on disk, because a three-way
merge does not: `git merge-tree --write-tree` (git 2.38+) does the whole thing
in the object database. Git agrees that rebasing should not need a worktree --
`git replay` (2.44+) exists for exactly this -- and we do not need autosquash's
help finding the target, because we were told it.

So `fixup` is: write the approved tree, synthesise a commit for it, merge it
into the target, replay the rest of the clean line, move the branch.

```sh
T=$(git write-tree)                                   # approved tree
F=$(git commit-tree $T -p $CLEAN -m fixup)            # the approval, as a commit
XT=$(git merge-tree --write-tree --merge-base=$CLEAN $TARGET $F)
XNEW=$(git commit-tree $XT -p $TARGET^ -m "<target's message>")
# ... replay each later clean commit the same way, then:
git update-ref refs/heads/clean $NEW_TIP
```

Tested:

```
clean:  cfa4c54 Add feature B   00fbff8 Add feature A   8f64ca4 start
  'Add feature A' a.txt = [A]

  -> approve the rest of a.txt, fixup into 'Add feature A'

clean:  5f34d56 Add feature B   94178ab Add feature A   8f64ca4 start
  'Add feature A' a.txt = [A, A more]
working tree byte-identical:  YES
file never rewritten (mtime unchanged):  YES
still unapproved: ?? junk.txt
```

Two clean commits before and after -- absorbed, not appended -- and the working
tree was not merely restored, it was never touched.

### Conflicts

A fixup conflicts when a later clean commit changed the same lines you are
folding into an earlier one. This is not an edge case to tolerate; it is the
normal cost of editing history you have already composed, and the tool has to
be good at it.

**A fold conflict implies a replay conflict.** Folding into `A` can only
conflict if some commit between `A` and the tip also touched those lines -- and
that commit will therefore conflict again when it is replayed onto the new `A`.
Two resolutions, not one. That is inherent, and `git rebase -i` has exactly the
same property; the difference is that we can see it coming and say so:

```
fixup: conflict folding into "A: f=two"  (step 1 of 2)
  f.txt

  "B: f=three" also changes f.txt, so this will conflict again on replay.
  If the change belongs there, abort and fix up "B: f=three" instead.

  resolve:  review resolve
  then:     review fixup --continue
```

That diagnostic is the useful part. Very often a fold conflict means the change
belongs in the later commit, and the tool knows which one because it is about to
replay it.

### Resolving

`merge-tree` hands back everything needed, in two forms: a tree in which the
conflicted files already contain ordinary conflict markers, and the three stages
as blobs. So `review resolve` can write

```
<scratch>/f.txt.BASE     <scratch>/f.txt.LOCAL     <scratch>/f.txt.REMOTE
```

and invoke the merge tool already configured in git (`mergetool.<tool>.cmd`).
For an IDE with a command-line launcher this opens a merge tab **in the window
already open** -- no second project, no worktree, no checkout. The resolved file
is hashed straight into the object database and recorded.

For anyone who prefers markers, the marked-up file is already in the conflicted
tree and can be dropped in the scratch directory instead.

### In progress

The state lives in `.git/review/fixup/`, the same shape as `.git/rebase-merge`:
the target, the list of commits still to replay, the partially rebuilt line, and
the current step's stages and resolutions.

Nothing is ever half-written. The clean branch moves once, by a single
`update-ref`, after every step succeeds. Which gives three properties a real
rebase cannot:

- **`--abort` is free.** Delete the state directory. There is nothing to undo,
  because nothing was written.
- **A crash is free**, for the same reason.
- **You can keep working.** The conflict lives in a scratch directory, not your
  working tree, so you can switch to write mode mid-fixup, write code, commit on
  the working line, and come back to it tomorrow. A repository in the middle of
  a rebase is a repository you cannot use; this one you can.

Tested end to end: a fixup into an earlier commit that conflicted at the fold
*and* on replay, resolved at both steps, produced the right clean line -- and
the working tree was not merely restored afterwards, its mtime never changed.

Recurring conflicts are likely, since the working line keeps moving and the same
fold gets retried. Resolutions are keyed by (base, ours, theirs), which is what
`git rerere` already caches, so reusing it is the obvious next step and should
wait until the repetition is actually felt.

## The tool

Five verbs. Everything else is git and your editor.

- `start` -- set up the clean line and the two index files.
- `write` / `review` -- toggle.
- `fixup <commit>` -- fold the approved changes into an existing clean commit,
  entirely in the object database; `--continue` and `--abort` when it conflicts.
- `resolve` -- open the current conflict in the merge tool git is already
  configured with.
- `refresh` -- when the working line is someone else's, pull its new tip into
  the working tree (below).

Committing, staging hunks, editing and resolving are all the editor's, natively.
In review mode the editor's own commit button does the right thing, because
"stage the hunks I approve, then commit them with a real message" is already
what it does -- it is just staging against a different branch.

## Done

The clean line is finished when review mode shows nothing: the clean tip's tree
equals your working tree. `git status` in review mode *is* the outstanding work.

That also forces a cleanup that is easy to skip otherwise. Debug junk never
reaches the clean line, but it also keeps the review list non-empty, so
finishing means deleting it from the working line rather than shipping around it.

## When the working line is someone else's

Nothing above cares who wrote the working line. If it arrived from a colleague,
or from an agent in a sandbox, the model is identical -- they produce the stream,
you curate.

The one addition is `refresh`, for when the working line moves under you:

```sh
GIT_INDEX_FILE=$H git read-tree -m -u "$OLD_TIP" "$NEW_TIP"
```

A two-way merge that updates the working tree and a helper index, leaving the
review index alone. `$H` is persistent, so it keeps the stat cache. Tested
against a commit that edited one file, deleted another and added a third:
approvals survived, deletions propagated, and a file with one approved hunk and
one new one showed as `MM`.

It refuses rather than clobbers when a file you edited also changed upstream,
which is the right behaviour -- that is a real conflict and should stop.

## Rough edges

- **Files added in the working line show as unversioned** in review mode, since
  HEAD is the clean line and does not know them. Staging fixes the display.
- **Approving a deletion means staging the deletion**, which reads oddly once.
- **There is no "never" bucket, by design.** Debug junk stays on the review list
  until it is deleted or commented out on the working line, which is where it
  should have gone anyway.
- **There is no record of rejection, by design.** Rejecting something means
  editing it; the end state is a change set that is entirely approved, with
  nothing outstanding to remember.
- **Whole-file rewrites** in the working line return the whole file to
  unapproved. Correct, occasionally tedious.
- **`refresh` does write files**, unavoidably -- it is the one operation that
  genuinely moves the working tree, because the working line moved.
- **Editors may or may not notice HEAD and the index changing underneath them.**
  Both are watched files and external git operations are normally picked up, but
  this is the one assumption here that has not been tested.

## Prior art

Nothing does quite this. The neighbours:

- **Stacked-diff tools** (`git-branchless`, Graphite) maintain a clean stack
  while you work, but assume you author the clean commits directly.
- **`git absorb`** works out *which* commit a change belongs in, which is the
  one thing `fixup` still asks you for.
- **`git replay`** (2.44+) is git's own acknowledgement that rebasing should not
  require a worktree. It has no autosquash, which is why `fixup` does the
  surgery with `merge-tree` directly.
- **Jujutsu** makes rewriting painless, but has one line rather than two.
- **`git add -p` into a dirty working tree** is the manual ancestor of all of it.
  The new part is giving the dirty side a history of its own, so a day's work is
  never referenced by nothing.
