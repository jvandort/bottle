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
rewritten freely -- which is what `git commit --fixup` plus an autosquash rebase
already does, and what many people already have as a shell alias. Approve the
new hunks and put them where they belong:

```sh
review fixup <clean-commit>
```

The catch is that a rebase checks out commits, and the working tree must stay
where it is. So the rebase happens somewhere else:

1. Write the approved tree from the index: `git write-tree`.
2. Make a `fixup!` commit for it on the clean line's tip, with `commit-tree`.
3. `git worktree add` a temporary detached worktree at that commit, and run
   `GIT_SEQUENCE_EDITOR=true git rebase --autosquash -i <target>^` **there**.
4. Point the clean branch at the result and remove the temporary worktree.
5. Re-read the index from the new clean tip, refreshing stat info so the
   working tree is left alone.

Tested:

```
clean line:  ecd07ca Add feature B   98d3a75 Add feature A   05746b5 start
  'Add feature A' has a.txt = [A]

  -> approve the rest of a.txt, fixup into 'Add feature A'

clean line:  678ecd0 Add feature B   b0d79b7 Add feature A   05746b5 start
  'Add feature A' has a.txt = [A, A more]
working tree untouched: YES
still unapproved: ?? junk.txt
```

Two commits before, two after -- the fixup was absorbed, not appended. The
working tree never moved, and the remaining unapproved change is still
outstanding.

The temporary worktree also gives conflicts a place to happen that is not your
working tree. When the autosquash conflicts, the tool reports where it stopped
instead of leaving your editor in a rebase.

## The tool

Five verbs. Everything else is git and your editor.

- `start` -- set up the clean line and the two index files.
- `write` / `review` -- toggle.
- `fixup <commit>` -- put the approved changes into an existing clean commit.
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
- **There is no "never" bucket.** Debug junk stays on the review list until it
  is deleted from the working line. A reject list would silence it, at the cost
  of somewhere else to keep state.
- **A fixup can conflict**, and then the clean line is mid-rebase in a temporary
  worktree. Recoverable, but it needs a clear message.
- **Whole-file rewrites** in the working line return the whole file to
  unapproved. Correct, occasionally tedious.
- **No record of what you rejected or why.** The index says what you accepted.
- **Editors may or may not notice HEAD and the index changing underneath them.**
  Both are watched files and external git operations are normally picked up, but
  this is the one assumption here that has not been tested.

## Prior art

Nothing does quite this. The neighbours:

- **Stacked-diff tools** (`git-branchless`, Graphite) maintain a clean stack
  while you work, but assume you author the clean commits directly.
- **`git absorb`** does the fixup-into-the-right-commit trick automatically, and
  would be a good `fixup` implementation when the target is not obvious.
- **Jujutsu** makes rewriting painless, but has one line rather than two.
- **`git add -p` into a dirty working tree** is the manual ancestor of all of it.
  The new part is giving the dirty side a history of its own, so a day's work is
  never referenced by nothing.
