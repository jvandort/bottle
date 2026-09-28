# Two lines: a review tool

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

Four verbs. Everything else is git and your editor.

- `start` -- set up the clean line and the two index files.
- `write` / `review` -- toggle.
- `fixup <commit>` -- fold the approved changes into an existing clean commit,
  entirely in the object database; `--continue` and `--abort` when it conflicts.
- `resolve` -- open the current conflict in the merge tool git is already
  configured with.

That is the whole surface, and it is worth defending. Anything a user can do by
switching to write mode and typing ordinary git does not need a verb here --
which rules out keeping up with someone else's working line (`git pull`),
committing work in progress (`git commit`), and committing what you have
approved (the editor's commit button, since HEAD is the clean line). The tool
exists to arrange the trees; git does the rest.

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
or from an agent in a sandbox, the model is identical -- they produce the
stream, you curate.

And keeping up with it needs no mechanism at all. Switch to write mode and use
git:

```sh
review write
git pull --rebase        # or fetch, or reset --hard, or whatever suits
review review
```

In write mode HEAD is the working line and the index is the write index, so this
is an ordinary repository doing an ordinary thing. **Conflicts are ordinary
working-line conflicts**, in the working tree, resolved in your editor the way
you always resolve them. None of the object-database machinery that `fixup`
needs applies here, because the working line's tree *is* checked out.

The review index is not involved and needs no adjustment, because it is a tree:
it records approved *content*, and content does not care how it got there.
Tested by rewriting the working line's history underneath a review -- a
different commit, a different message, one line changed -- and toggling back:

```
approved earlier:  the change on line 1
after the rewrite: status  M f.txt   ?? g.txt
unapproved diff:   -b +B-new
```

Only the genuinely new change came back. The approved line did not reappear.

So the answer to "is this a fast-forward, could I have just pulled?" is yes, and
you literally do. If you have no commits of your own on the working line it is a
plain update. If you do, it is a plain rebase, with plain conflicts. Either way
only the working line and the working tree move; the clean line and the review
index are untouched.

### Never merge into uncommitted work

Git already refuses: `git rebase` with a dirty tree stops with "cannot rebase:
your index contains uncommitted changes", and `rebase.autoStash` is off unless
you turn it on. That default is right and the tool should not override it.

The escape is to **commit, not to stash**. A stash is recoverable but it is a
single global slot, an applied stash that conflicts hands you two problems at
once, and a dropped one exists only in the reflog. The working line, meanwhile,
is a history nobody reads, where an extra commit costs nothing, is named by a
branch ref, survives a crash, and comes back exactly from `git rebase --abort`.
On a line whose history does not matter, autocommit beats autostash.

So when the tree is dirty, commit on the working line and then pull -- in write
mode, where both are ordinary git:

```sh
review write
git add -A && git commit -m wip
git pull --rebase
```

The tool owns none of that, and should not. Every route out of review mode goes
through write mode anyway, and from write mode this is a repository doing
ordinary things.

There is deliberately no `wip` verb. Doing it from review mode would only save a
round trip through a mode switch that costs two renames, and the mistake it
would guard against -- `git add -A` in review mode meaning "approve everything
unread" -- is one the rule already prevents. For work you forget to commit at
all, the answer is a snapshot taken from outside on a timer (`../docs/design/durability.md`),
not a verb you have to remember.

## Going away and coming back

A common workaround, without two lines, is to make one temporary commit of the
staged changes and a second of the unstaged ones -- preserving the
reviewed/unreviewed split through a branch switch -- then undo both on return.

That trick is unnecessary here, because **the split is not in the working tree
to begin with**. It lives in two places, neither of which a branch switch
touches:

- what you have approved is the review index, a file;
- everything else is the difference between that and the working tree, and once
  you are in write mode the working tree's content is committed on the working
  line like any other work.

So there is nothing to encode in temporary commits, and nothing to undo on
return. A `wip` commit is just more history on a line whose history nobody
reads.

```sh
review write      # park the approved set; now an ordinary repository
git commit -am wip    # if the tree is dirty
git switch elsewhere
# ... later ...
git switch <working line>
review review     # approved set back, exactly as it was
```

**Leaving review mode is not optional, and committing does not substitute for
it.** In review mode the working tree deliberately differs from HEAD -- that
difference is exactly what "unstaged means unreviewed" is made of. So the tree
is never clean in review mode, not because there is uncommitted work lying
around, but by construction. Committing the approved half to the clean line and
`wip`-ing the other half to the working line still leaves it:

```
both halves committed, still in review mode:
  status:  M f.txt
  git switch elsewhere
  -> error: Your local changes to the following files would be overwritten

switch to write mode first:
  status:  (clean)
  git switch elsewhere
  -> Switched to branch 'elsewhere'
```

In write mode HEAD is the working line, the working tree matches it, and a
checkout is an ordinary checkout. Getting there is two renames and a
`symbolic-ref`, so it costs nothing.

There is a second reason, worse than inconvenience. The live `.git/index` *is*
the approved set, and a checkout writes the index, so what happens depends on
the branch you are switching to:

```
target branch has the same content:   Switched to branch 'elsewhere'
                                      (approvals silently carried over, now
                                       recorded against an unrelated branch)
target branch differs:                error: Your local changes to the following
                                      files would be overwritten by checkout
```

Refusing is the good case. Silently succeeding leaves you with an approved set
that means nothing, and no sign anything happened.

### The approved set belongs in a ref

The index file is the working copy of the approved set, and the reason it exists
is the stat cache that makes the editor fast. It should not be the only copy.

Record it as a ref too -- `refs/review/<clean branch>`, a commit whose tree is
the approved tree -- written whenever the tool runs. Then the index file is a
cache that can be rebuilt:

```sh
git read-tree refs/review/clean
git update-index --refresh
```

Verified: deleting the index entirely and rebuilding it from the ref gives back
the identical approved tree.

That also makes approval history real, because a ref has a reflog: what you had
approved before lunch is `refs/review/clean@{1}`. The same rule as everywhere
else in these documents -- **state that matters goes in a ref**, and anything
else is a cache.

The gap is approvals made in the editor between tool invocations, which are not
captured until the next one. A mode switch captures them, and a mode switch is
what you do before leaving, so the exposure is small; a `post-checkout` hook
that notices the previous HEAD was the clean line could close it entirely.

## One review per branch

A single review index is a limitation, and an odd one, because the durable half
of the state is already per-branch: `refs/review/<clean branch>` is keyed by the
branch it belongs to. Only the index *cache* is singular. Making it per-session
is aligning the cache with the record rather than adding a concept.

**It buys resumability, not concurrency.** The working tree is shared, so you can
only ever be looking at one branch's content; two reviews at once would need a
worktree each, which is the two-window design that was rejected. What per-branch
state gives you is that switching away does not lose your place in either
review -- which matters as soon as more than one agent is pushing branches.

A session is a (working line, clean line, review index) triple, keyed by branch:

```
.git/review/sessions/<encoded clean branch>/
    working            the working line's ref name
    index.review
    index.write
```

Looking up the current session needs no "which one is active" pointer, because
HEAD already says: in review mode it is the clean branch, in write mode the
working line, and each names exactly one session. Switching branches in write
mode therefore moves to a different session by itself.

**The index files are a pure cache.** If one is missing or stale, rebuild it from
the ref with `read-tree` and `update-index --refresh`; the only thing lost is the
stat cache, which costs one re-stat -- the same scan `git status` does. So
sessions degrade gracefully and old index files can be deleted freely.

Two things this needs:

- **Encode the branch name.** `refs/review/agent/foo` and `refs/review/agent`
  cannot both exist -- git refuses with "cannot create; 'refs/review/agent/foo'
  exists" -- and both are legal branch names. Percent-encode `/` (and `%`) so
  every session is one ref path segment.
- **`review list` and `review drop`**, because branches get deleted and reviews
  get abandoned, and nothing else will ever clean them up.

## Guardrails

### The hazard peaks when the review is finished

Git protects you from switching branches in review mode by accident, but only as
a side effect: the working tree differs from HEAD, so a checkout that would
overwrite it refuses. **That protection exists because there is unreviewed work,
so it disappears at the exact moment there is none.** Approve and commit the last
hunk and the tree is clean, and `git switch` succeeds without a word -- which is
also the moment you are most likely to switch away, because you are done.

So the one place to say something out loud is when a review completes.

### What actually breaks

The checkout itself is survivable. The damage happens afterwards:

```
review complete, status clean, still in review mode
  git switch elsewhere        -> Switched to branch 'elsewhere'
  HEAD:          refs/heads/elsewhere
  tool thinks:   mode=review
  live index is: elsewhere's tree, not the approved one

  then a mode switch, done blindly:
  index.review now holds elsewhere's tree      <- the approved set is gone
  worktree holds elsewhere's content, HEAD says working line
```

Losing the approved set happens when the *tool* renames the live index over
`index.review`, not when git checks out. Which means it is entirely preventable.

### The invariant

Every command checks one thing before touching anything:

> HEAD is the branch the recorded mode says it should be.

If it is not, we were moved out from under the tool. Refuse to rename any index
file, say what happened, and offer to recover. This single check converts silent
data loss into a clear error, and it costs one `symbolic-ref`.

### Recording approvals as they happen

`refs/review/<clean branch>` should be written continuously, not just when the
tool runs, or approvals made in the editor since the last invocation are the
thing that gets lost.

Git's `post-index-change` hook fires on every index write -- verified firing for
`git add` *and* for `git apply --cached`, which is how an editor stages a single
hunk. Guarded by "mode is review and HEAD is the clean branch", it records the
approved tree on every stage, so an approval is durable the instant it is made.

One ordering subtlety the implementation has to get right and verify: during a
checkout, `post-index-change` fires too, and HEAD may already have moved or not.
The safety net is that the ref has a reflog, so `post-checkout` -- which knows
the old and new HEAD -- can detect the yank and roll the ref back one entry.

### `post-checkout`

The only hook that fires on `git switch`. It cannot prevent anything, since it
runs afterwards, but it can notice that the old HEAD was the clean branch while
the mode said review, mark the state `away`, restore the review index from the
ref, and print what happened and how to get back. The difference between
discovering this now and discovering it in a week.

### Smaller things

- **`review status`** -- mode, both lines, how much is outstanding, and whether
  the invariant holds. The command you run when something feels off.
- **`review end`** -- an explicit exit to write mode, so leaving is a thing you
  do rather than a thing you forget.
- **Name the clean branch distinctively.** The editor's branch widget is already
  a mode indicator, for free, if the two names are not easily confused.

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
- **Updating a working line someone else writes does move files**, unavoidably,
  because the working line moved. It is the only operation that does.
- **Editors may or may not notice HEAD and the index changing underneath them.**
  Both are watched files and external git operations are normally picked up, but
  this is the one assumption here that has not been tested.

## Implementing it

`IMPLEMENTATION.md` has the state layout, the algorithms, the exact
plumbing, the edge cases, and a glossary of every git concept the implementation
depends on.

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
