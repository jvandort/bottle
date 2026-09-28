# Review state: the staging area, as a ref

Status: design. Not built. The mechanism below is tested; the command is not
written.

Companion to `reviewing-a-bottles-work.md`, which covers where a bottle's work
surfaces and where you edit it. This document covers one narrower question:
**how do you keep track of what you have already reviewed, when the thing you
are reviewing keeps changing underneath you?**

## `bottle review`

The command first, because the mechanism only matters if the command is right.

```sh
bottle review NAME [BRANCH]      # approve hunk by hunk, from where you left off
bottle review NAME --diff        # what is still unreviewed
bottle review NAME --status      # one line per branch: reviewed, outstanding
bottle review NAME --reset       # start this branch's review over
```

`bottle review gradle` walks the difference between **what you last approved**
and **what the branch says now**, hunk by hunk, the same interaction as
`git add -p`. Approve some, skip some, quit whenever. Nothing about the repo
changes: no commit is made, no branch moves, and your own staging area is not
touched.

Come back after the agent has pushed three more times and it picks up where you
stopped. Anything the agent changed in a file you had already approved shows up
again, because it is genuinely new. Everything else stays approved.

If you fix the wording yourself instead of asking, that edit is reviewed by
definition -- you wrote it -- and approving it is the same gesture as approving
mine.

`--diff` is the one you would run most: *what have I not looked at yet.*

## Why not the staging area

Using the index as "the reviewed set" is the best version of this that exists
without new tooling, and it works because of four properties that are hard to
get all at once:

| property | what it buys |
| --- | --- |
| **Content-based, not history-based** | the index holds a tree, so rebases, amends and squashes -- which agents do constantly -- do not disturb it |
| **Hunk granularity** | you can approve half a file |
| **Monotone under change** | if the agent touches a file you approved, only the new delta comes back; the rest stays approved |
| **Editing is approving** | fix the wording, stage it, done -- no round trip |

It breaks here for one reason: **the index is a single global mutable slot, and
the agent runs git too.** `git add -A`, `git commit -a`, `git stash`,
`git checkout` -- any of them destroys the review state silently. Instructing
agents not to touch the index works, but it is a convention protecting
unprotected state, and it fails the first time an agent reaches for `git stash`
to get out of trouble.

Two smaller limits: one index means one review at a time (the usual workaround
is two throwaway commits and a branch switch), and there is no record of what
you approved last session.

## Why not IDE changelists

JetBrains changelists give you buckets, but they are not git. You cannot commit
"the things I approved" as a unit, which is the whole point of using the index
for this -- staging what belongs in this commit and leaving the rest for the
next one is the same gesture as marking it reviewed. A parallel, non-git
bucketing system means maintaining the split twice.

## The mechanism

Git lets you point at a different index file, and lets you persist a tree as a
ref. That is the whole idea:

```sh
IDX=.git/bottle-review/gradle-work            # your index, not the repo's
GIT_INDEX_FILE=$IDX git read-tree <last-approved-tree>
GIT_INDEX_FILE=$IDX git add -p                # approve hunks
TREE=$(GIT_INDEX_FILE=$IDX git write-tree)
git update-ref refs/bottle/reviewed/gradle/work \
    $(git commit-tree $TREE -p refs/bottle/reviewed/gradle/work -m reviewed)

git diff refs/bottle/reviewed/gradle/work     # everything not yet reviewed
```

Tested end to end:

- Approving one file left the agent's `git status` completely untouched.
- The agent then committed, `--amend`ed, and committed again; the review ref
  survived all of it.
- When the agent re-touched an approved file, that file came back as unreviewed
  while the others stayed approved.
- Editing a file by hand and staging it into the review index counted as
  reviewed.
- `git add -p` honours `GIT_INDEX_FILE`, so hunk granularity is kept.

So all four properties above survive, and the failure mode is gone: the agent's
git never sees this index file. It adds three more:

- **Concurrent reviews.** One ref and one index file per bottle per branch.
- **Durable.** It is a ref, with a reflog, so "what did I approve on Tuesday"
  is answerable and a review survives closing the laptop.
- **Your index is free again.** The staging area goes back to its real job --
  composing the next commit -- instead of doing two jobs badly.

Storing review state as a commit rather than a bare tree costs nothing and buys
a reflog and a parent chain, so each review pass is a recoverable point.

## Crash course: reviewing with git when you are used to an IDE

The mechanism above assumes some git-side review habits. Here is the minimum.

**Three trees, not two.** git always has `HEAD` (last commit), the index
(staged), and the working tree (files on disk). `git diff` is *index vs working
tree*. `git diff --cached` is *HEAD vs index*. `git diff HEAD` is both together.
`bottle review` adds a fourth tree that is none of these -- your approved set --
and `git diff <that ref>` means *approved vs working tree*. Any tree-ish works
on the left of `git diff`; a ref is just a name for one.

**`git add -p` keys.** `y` approve this hunk, `n` skip it, `s` split it into
smaller hunks, `e` edit the hunk by hand (the precise tool when `s` will not
split far enough), `q` stop, `?` help. `s` and `e` are the two worth learning;
everything else is obvious after one session.

**Use your own differ, not the terminal.** This is the part that makes git-side
review bearable if you are used to a graphical diff. Configure the IDE as git's
difftool once:

```ini
[diff]
    tool = intellij
[difftool "intellij"]
    cmd = idea diff "$LOCAL" "$REMOTE"
```

Then `git difftool <ref>` opens single files and `git difftool -d <ref>` opens
the whole change as a directory diff, with syntax highlighting and everything
else you expect. (`idea` is JetBrains' command-line launcher, installed from
Toolbox or the IDE's "Create Command-line Launcher". Not tested from inside a
bottle -- this is a Mac-side setting.)

The split worth internalising: **read in the IDE, approve in the terminal.**
`git difftool -d refs/bottle/reviewed/...` to see what is outstanding, then
`bottle review` to mark off what you accept.

**`git range-diff` for rewritten history.** When the agent rebases or squashes,
an ordinary diff between two versions of a branch is noise. `git range-diff
old...new` shows which *commits* changed and how -- "commit 2 is unchanged,
commit 3 gained two lines". There is no IDE equivalent, and it is the right
tool for "the agent says it addressed my feedback, what actually moved".

Note that the reviewed-tree model makes this mostly unnecessary for the
approval question itself: content is content, so a rebase that changes nothing
leaves nothing unreviewed. Range-diff is for understanding, not bookkeeping.

**What replaces the two-throwaway-commits trick.** Nothing, because the problem
goes away: review state lives in a ref per branch, so reviewing two branches at
once needs no branch switching and no temporary commits.

**What does not change.** Your normal staging habits. `git add -p` into the real
index to compose one commit and leave the rest for the next one still works
exactly as it does today -- it is a different index file, so the two never
interact.

## Limits and risks

- **Whole-file rewrites.** If the agent rewrites a file you had partly approved,
  the entire file comes back as unreviewed. Correct, and occasionally annoying.
- **`add -p` does not cover everything.** Binary files, mode changes and renames
  are approved whole or not at all.
- **No record of *why*.** The ref says what you accepted, not what you objected
  to. In-file `REVIEW:` markers or ordinary chat still carry the reasoning.
- **Staleness.** A review ref outlives the branch it describes, the same way a
  bottle's refs outlive the bottle. `--reset` handles it manually; automatic
  cleanup is the same problem as `bottle prune`.
- **Single reviewer.** The ref is local to the host repo. Sharing a review with
  someone else is out of scope and probably should stay there.
- **It is not a substitute for tests.** Approving a hunk means you read it, not
  that it works.
