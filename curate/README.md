# curate

Write on one branch, review and squash on another, using the git tools you
already know.

Most git workflows make you pick: commit as you go and tidy up later with an
interactive rebase, or hold everything uncommitted until you can compose it
properly. `curate` keeps two lines at once.

- **The working line** is where you write. Messy commits, any order, rebased
  and squashed whenever you like. Nobody needs to read its history, including you.
- **The clean line** is what you ship. Every commit is deliberate, built from
  changes you included on purpose.

Switching between them moves `HEAD` and the index. **It never touches your
files**, so your IDE never re-indexes, and your files always remain committed
in the working line.

## Install

Python 3.11+, git 2.38+ (for `merge-tree --write-tree`), no dependencies. It
shells out to git for everything.

`bin/curate` beside this directory is the command -- a launcher that checks your
interpreter and hands off to the package here. Put that directory on your
`PATH`:

```sh
export PATH="/path/to/repo/bin:$PATH"
```

Linking it into a directory already on your `PATH` works too -- it resolves
itself through the symlink.

## Quick start

```sh
curate start          # asks where the clean line should begin
curate review         # now you are reading
```

In review mode your editor is the tool. Stage a hunk to approve it; commit to
put it on the clean line. Unstaged means unreviewed, so `git status` **is** your
review to-do list, and the review is done when it is empty.

That includes files the working line added: the clean line does not know them,
so they show up as untracked (`??`). Unread is unread either way. (`git diff`
does *not* show them, which is why `git status` is the list and `git diff` is
not.)

```sh
curate write          # back to writing code
```

## Commands

A summary; `curate --help` is the real thing.

```
curate                       status here, or the command list if no review
curate start --from <base>   begin. --clean <branch> to name the clean line
curate review                switch to review mode
curate write                 switch back
curate switch                toggle between the two
curate status                report on curate's state, and anything wrong
curate fixup <commit>        fold what you approved into an existing clean
                             commit.  --continue / --abort when it conflicts
curate resolve               open the current fixup conflict in git's merge tool
curate list                  every review in this repository
curate drop <branch>         forget a review started by `start`
```

`curate status --porcelain` prints `key=value` lines for scripts.

## What the two modes show you

|            | `HEAD` is        | staged means        | unstaged means      |
| ---------- | ---------------- | ------------------- | ------------------- |
| **write**  | the working line | ordinary staging    | your uncommitted edits |
| **review** | the clean line   | **approved**, not yet committed | **unreviewed** |

Your files are identical in both. Only `HEAD` and the index move.

So in review mode, `git status` shows the clean line's tip against your actual
files: everything you have not yet read. In write mode `HEAD` is the working
line instead, so `git status` is back to showing your uncommitted edits, and
the clean line is just another branch you are not on.

## Naming

The clean line for `branch` is `curate/branch`. Every clean line in a repository
sorts together, away from branches people work on, and your editor's branch
widget becomes a mode indicator for free.

Refs are paths, so `refs/heads/curate` and `refs/heads/curate/anything` cannot
both exist: one is a file where the other needs a directory. So **a branch named
exactly `curate` rules out the default naming for every review in that
repository** -- not just its own.

So **if you use curate, you cannot have a branch named `curate`.** `curate
start` refuses while one exists, even if you name the clean line yourself 
with `--clean`. The resolution is to rename your branch.

```sh
git branch -m curate something-else
```

## Approving

There is no `approve` verb, on purpose. Staging is approving, and your editor
already does that well:

```sh
curate review
git add -p f.py              # approve some hunks
git add f.py                 # approve the whole file
git commit -m "Add the thing"   # put the approved set on the clean line
```

Approval is **content**, not history. Rebase or squash the working line as much
as you like: a rewrite that changes no content leaves nothing unapproved.

## Fixing up a clean commit

If you already committed changes to the clean line but want to update that commit,
use the `fixup` command:

```sh
git add f.py                 # approve the new version
curate fixup <the clean commit>
```

It folds the approved changes into that commit and replays the rest of the clean
line on top, **entirely in the object database**. No checkout, no stash, no temporary
worktree -- your working tree is not restored afterwards, it is never touched. All
unapproved changes remain unapproved.

When `fixup` conflicts, the conflict is written as files under
`.git/curate/sessions/<clean>/fixup/stages/` -- the three sides and a marked-up
file -- and you resolve it there, not in your working tree.

```sh
curate resolve               # opens git's configured merge tool on them
curate fixup --continue
curate fixup --abort         # free: nothing had been written
```

With no merge tool configured, `resolve` still writes those files and tells you
where; edit the marked-up one by hand and `--continue` reads it back. Either
way `--continue` refuses while any of them still has conflict markers in it,
and names the files -- the clean line is the one branch that exists to have
none.

Folding into an old commit conflicts when a later one touched the same lines,
which means it will conflict again on replay. That is two resolutions, not one.
It is inherent -- `git rebase -i` has the same property -- and curate tells you
which later commit is coming, so you can abort and fix that one up instead.

**You can keep working while a fixup is paused.** The conflict lives under
`.git/curate/`, not in your working tree, so `curate write`, write code, commit,
and come back to it tomorrow. A repository mid-rebase is one you cannot use;
this one you can.

Two reviews can be paused on a conflict at the same time, on different
branches. The state is per review, so neither is in the other's way, and
finishing one does not disturb the other.

## Keeping up with a branch someone else writes

An agent's branch, a colleague's, your own from another machine: switch to write
mode and use ordinary git.

```sh
curate write
git pull                     # plain conflicts, in your working tree, as usual
curate review                # your approvals come back untouched
```

They come back because the approved set is a *tree*: it records content, and
content does not care how it got there. Only genuinely new changes reappear as
unapproved. If your tree is dirty, commit on the working line -- never stash.
It is a history nobody reads, where an extra commit costs nothing.

## Starting from the wrong base

You often cannot tell until review mode shows you a list that is far too long
or far too short.

```sh
curate drop <your branch>    # deletes the clean branch too, if nothing was approved onto it
curate start --from <the right one>
```

Once you have committed something to the clean line, `drop` leaves the branch
alone and tells you how to delete it by hand.

## Leaving

**You can switch branches straight out of review mode.** Once a review is
finished, git no longer refuses the checkout, and curate treats it as though
you had run `curate write` first: it stands the review down.

Nothing is lost, because nothing was only in the index git just rewrote. The
approved set is durable in `refs/curate/<clean branch>`, written on every stage
by a `post-index-change` hook, and that ref has a reflog. What curate refuses
to do is the dangerous half: treat the index git left behind as your approvals.

In write mode there is nothing to move out from under: the approved set is
parked, and the live index is git's own. Either way, switching to a branch
with no review of its own is ordinary -- curate says there is none here,
`curate start` begins one, and coming back to the working line picks the first
one up again with your approvals intact. HEAD is what says which review you
are in.

## Several reviews at once

One review per branch, kept separately, so switching away does not lose your
place in either. `curate list` shows them; `curate drop` forgets one.

You can only *look* at one at a time, because there is one working tree.

## Where it keeps things

```
.git/curate/mode, active          which mode, which review
.git/curate/sessions/<clean>/     one per review: the two index files, fixup state
refs/curate/<clean branch>        the approved set, as a real commit, with a reflog
```

The index files are a cache. Delete them and the next command rebuilds them
from the ref; all you lose is a re-stat. curate installs three hooks
(`post-index-change`, `post-checkout`, `post-commit`) and leaves any you
already have alone -- `curate status` then reports that one is in the way,
every time, and prints the line to add to it. It says so every time rather than
once, because git-lfs and friends put a hook here as a matter of course, and
what you lose is invisible until you need it.

Each hook holds an absolute path to curate, so moving or renaming your checkout
leaves it naming nothing. The hook checks before running, so a stale one is
silent rather than an error on every commit, and any curate command rewrites
it.

## More

- `CONTRIBUTING.md` -- the tests, the conventions, and how it is built.
- `TODO.md` -- what is missing, and the rough edges worth knowing about.
