# Reviewing with a worktree

Status: design, tested as a mechanism. Not built, and **not part of bottle** --
see the layering note in `reviewing-a-bottles-work.md`. This lives here until
the tool it describes has a repo of its own.

The problem: an agent pushes a branch, rewrites it, pushes again, and you need
to keep track of what you have already read -- by hunk, in your IDE, without
leaving it.

## The arrangement

Git has three trees. Point them at the right things and the review is just
`git status`:

| tree | holds | shown in IJ as |
| --- | --- | --- |
| `HEAD` | the base commit the agent branched from | -- |
| index | what you have approved | **Staged Changes** |
| working tree | the agent's current tip | **Changes** (the rest) |

So **staged means reviewed** and **unstaged means not yet**. Staging a hunk in
the IDE is approving it. There is no new concept, no second tool, no terminal.
It is the staging-area workflow you already use, pointed at someone else's
commits instead of your own edits.

It has the properties that make the staging-area trick good in the first place:

- **Content-based.** The index holds a tree, so a rebase, amend or squash that
  changes nothing leaves nothing unreviewed.
- **Hunk granularity**, because it is the real index.
- **Monotone.** If the agent re-touches a file you approved, only the new delta
  comes back.
- **Editing is approving.** Fix the wording, stage it, done.

## Why a separate worktree

You can do this in the main repo -- `git checkout <agent-tip>` then
`git reset --mixed <base>` produces the same arrangement. Four reasons not to:

1. **One index per worktree.** While reviewing, the main repo's index is the
   review set, so it cannot also be doing its real job of composing your next
   commit. A worktree has its own index file
   (`.git/worktrees/<name>/index`), so the two never compete.
2. **HEAD has to be at the base commit.** In the main repo that means detaching
   from your branch and parking whatever you were doing.
3. **The files are the agent's, not yours.** Every refresh rewrites the working
   tree to the agent's latest. You do not want that happening to the checkout
   you are working in.
4. **One review at a time** in the main repo; one worktree per branch under
   review otherwise.

The cost is a second IDE project window and a second checkout on disk. Both
windows are the IDE, so this is not the read-here-approve-there split that makes
terminal review unbearable -- you review in one window and work in the other,
and never at the same moment.

## Setup

```sh
git worktree add --detach "$REVIEW" "$TIP"   # worktree content = the agent's tip
git -C "$REVIEW" reset -q --mixed "$BASE"    # HEAD and index back to base
```

`reset --mixed` moves HEAD and the index without touching the files, which is
exactly the arrangement above: everything the agent did shows as unstaged.

A helper index tracks what the worktree currently holds, so refreshes can update
the files without touching the review index:

```sh
GIT_INDEX_FILE=$H git -C "$REVIEW" read-tree "$TIP"
GIT_INDEX_FILE=$H git -C "$REVIEW" update-index --refresh -q
```

Keep `$H` around between runs; it holds the stat cache that makes refreshes
cheap.

## Refresh, when the agent pushes again

```sh
GIT_INDEX_FILE=$H git -C "$REVIEW" read-tree -m -u "$OLD_TIP" "$NEW_TIP"
```

A two-way merge that updates the working tree and the helper index, and leaves
the review index alone. Tested against an agent commit that edited one file,
deleted another and added a third:

```
files:     added.txt f.txt          (gone.txt removed from disk)
approved:  -1 +AGENT-1              (survived the refresh)
status:    MM f.txt    D gone.txt   ?? added.txt
```

`MM` on one file is the point: the hunk you approved stays staged while the
agent's new hunk in the same file shows up as unstaged.

`read-tree -m -u` refuses rather than clobbers when a file you have edited also
changed upstream. That is the right behaviour -- it is a real conflict, and it
should stop and say so instead of silently discarding your edit.

## Committing your edits

HEAD sits at the base commit, so the IDE's commit button would build the wrong
thing. Committing your edits onto the agent's branch is the one operation the
tool has to provide:

```sh
TREE=$(GIT_INDEX_FILE=$H git -C "$REVIEW" write-tree)
git commit-tree "$TREE" -p "$TIP" -m "$MESSAGE"
```

The result is an ordinary commit on the agent's branch, which the agent fetches
and rebases onto. Your edits are approved by construction -- you wrote them --
so the tool stages them into the review index at the same time.

## The whole tool

Three verbs:

- `start BRANCH` -- create the worktree and the two indexes.
- `refresh` -- pull the agent's new tip into the working tree, keeping approvals.
- `commit -m MSG` -- put your edits on the agent's branch.

Everything else -- reading, editing, staging hunks, resolving -- is the IDE,
natively. That is the point: the tool's job is to arrange the trees and then get
out of the way.

## Known rough edges

- **New files show as unversioned.** HEAD and the index are at the base commit,
  so a file the agent added is untracked rather than added, and appears under
  Unversioned Files rather than Changes. Staging it fixes the display. Fixable
  by seeding the review index with the paths rather than the content, at the cost
  of complexity.
- **Deletions are approved by staging them**, which reads oddly the first time.
- **A rebase that genuinely changes content** brings the changed hunks back as
  unreviewed, correctly. Understanding *why* they changed is what
  `git range-diff` is for, and no IDE has a view of it.
- **Whole-file rewrites** return the whole file to unreviewed. Correct,
  occasionally tedious.
- **No record of what you objected to.** The index says what you accepted.
  Reasons go in commits, comments in the code, or chat.
- **Binary files, modes and renames** are approved whole or not at all.

## Why not IJ changelists

Changelists bucket uncommitted changes, which is the right shape, but enabling
the staging area disables partial (per-hunk) changelists -- the two features are
mutually exclusive. With staging on, changelists are file-level, which is too
coarse for review. They are also not git: the buckets do not survive anything,
and nothing else can read them.

The arrangement above gets the same buckets from git itself, so the state is a
tree, survives restarts, and any tool can read it.

## Why not a plugin

A plugin is possible -- JetBrains' bundled GitHub plugin renders inline comment
threads in the gutter of a diff against a remote branch, and it is open source,
so the hard parts have a working template. But it would duplicate a UI the IDE
already has. The arrangement above needs no plugin because it makes review look
like the thing the IDE is already good at.

Worth reaching for a plugin only if the two-window split proves worse in
practice than it looks on paper.
