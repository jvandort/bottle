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
curate start          # fetches, asks which branch, then where its clean line begins
curate read           # now you are reading
```

Which branch is a list of every branch, local and fetched, newest first, with
the one you are on as the default. One that only exists on a remote becomes a
local branch tracking it; one that already has a review is switched back to
rather than started again. Give `start` any argument and it asks nothing about
the branch: the one you are on is the working line.

In read mode your editor is the tool. Stage a hunk to approve it; commit to
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
curate start --from <base>   begin. --clean <branch> to name the clean line,
                             --copy <branch> to copy that branch's review
curate read                  switch to read mode; --copy <branch> begins
                             this branch's review as a copy of that one's.
                             With no review here, asks which branch to start,
                             as a bare `curate start` does
curate review <branch>       pause the review you are in, and read <branch>'s,
                             beginning it if need be (--from <base>)
curate write                 switch back
curate switch                toggle between the two
curate git <command>         run a git command in write mode, whatever mode
                             you are in, then come back
curate status                report on curate's state, and anything wrong
curate fixup <commit>        fold what you approved into an existing clean
                             commit.  --continue / --abort when it conflicts
curate resolve               open the current fixup conflict in git's merge tool
curate transfer [<branch>]   copy this review onto the branch that replaced
                             its working line: fetch, choose, copy, come back
curate update-base [<base>]  move this review onto a new base, for a working
                             line rebased in place
curate integrate             ship a finished review onto a branch, new or
                             fast-forwarded, and end the review
curate list                  every review in this repository
curate drop [<branch>]       forget a review started by `start`; the one
                             you are in unless you name a branch
```

`curate status --porcelain` prints `key=value` lines for scripts.

## What the two modes show you

|            | `HEAD` is        | staged means        | unstaged means      |
| ---------- | ---------------- | ------------------- | ------------------- |
| **write**  | the working line | ordinary staging    | your uncommitted edits |
| **review** | the clean line   | **approved**, not yet committed | **unreviewed** |

Your files are identical in both. Only `HEAD` and the index move.

So in read mode, `git status` shows the clean line's tip against your actual
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
curate read
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

An agent's branch, a colleague's, your own from another machine: it is ordinary
git on the working line, so `curate git` runs it for you.

```sh
curate git pull              # plain conflicts, in your working tree, as usual
```

That is write mode, the command, and read mode again -- borrowed for exactly
as long as the pull takes. It works the same from write mode, where it is
simply `git`, so you never have to know which mode you were in to type it.
`log`, `fetch`, `diff`, `status`: anything that wants HEAD to be the
branch you are writing rather than the clean line.

You can still do it by hand, which is all `curate git` does:

```sh
curate write
git pull
curate read                  # your approvals come back untouched
```

They come back because the approved set is a *tree*: it records content, and
content does not care how it got there. Only genuinely new changes reappear as
unapproved. If your tree is dirty, commit on the working line -- never stash.
It is a history nobody reads, where an extra commit costs nothing.

If the command checks something else out -- `curate git switch elsewhere` --
there is no review to come back to, because the working line is no longer where
you are standing. curate says so and leaves you there; `git switch <working>
&& curate read` returns.

## When the branch is replaced rather than updated

Not every rebase arrives as a force-push. Whoever writes the working line may
not be able to rewrite what they have pushed -- a protected remote, a policy,
a sandboxed agent -- and push the rebase as a new branch instead: `feature-2`
where you were reviewing `feature-1`. The review belongs on the new one, so
copy it there:

```sh
curate transfer
```

That fetches the remote the working line tracks (every remote, if it tracks
none), and lists that remote's branches, newest first -- the one that was just
pushed is usually at the top. `<show local>`, at the bottom, lists your own
branches instead. Then it switches to the branch you choose, copies the review
onto it, and leaves you in whichever mode you were in. Nothing changes until
every question is answered, so `q` at any point leaves everything as it was.

By hand, it is:

```sh
git switch feature-2
curate read --copy feature-1
```

The clean line and your approvals come across, and since approval is content,
what the rebase changed is exactly what shows up as unreviewed. It is a copy:
the review of `feature-1` is left exactly as it was, so if the copy is wrong,
`curate drop` it and nothing else is touched. The old review stays until the
new one is integrated, which ends both.

**If the rebase moved onto a newer base**, the clean line is replayed onto it
and your approvals merged across, so what upstream did never lands on your
list. The copy never stops on a conflict. A clean commit that conflicts ends
the replay there: it and the commits after it come across as approved but not
committed, for you to commit again. An approval that conflicts is simply
unreviewed again. Both are still approved in the old review if you want to
look.

When the new branch looks to be on a different base than the old review --
judged by where it left the old base's branch, that branch's upstream, and the
usual mainlines -- curate asks rather than assumes, with the old base and the
branches on it named above the list. Off a terminal it refuses, and `--from`
says.

## When the branch is rebased where it stands

Rebase the working line itself onto a newer base -- `git rebase origin/main` on
the branch under review -- and the review's base is still the old one, so
everything upstream did since shows up as unreviewed. Move the review with it:

```sh
curate update-base origin/main
```

It is the copy above, onto the same branch: the clean line is replayed onto the
new base, your approvals are merged across, and the new base is recorded, with
the same fallbacks when something conflicts. With no base named, a terminal
gets the same list as a copy, the current base marked. You come back in the
mode you were in, and it prints the command that goes back to the old base.

## Shipping a finished review

```sh
curate integrate
```

A pull request without the pull request. What ships is the clean line if you
curated one, and the working line if you did not -- because the history was
already the way you wanted it, whoever made it so.

It ships only what you finished. Anything unreviewed is refused. So is a
change in your files that is not committed on the working line, even an
approved one: whoever writes the working line has not seen it, so it goes
back to them first. A clean line holding some of what you approved, but not
all, is refused too; commit the rest there.

Commits by anyone else are yours to decide on, author by author: adopt them,
rewritten as yours with their original dates and signed if your commits are,
or keep them as theirs. With one other author that is a single enter. With
several, every author has a checkbox, and the list can only be confirmed from
its last row, so each is a decision you made. Off a terminal, `--adopt
<author>` and `--no-adopt <author>` have to name every one.

Then where: `<new branch…>`, first, starts a branch at the review's base, and
below it is every branch the line can fast-forward, with the one the base came
from marked. `--into <branch>` says without asking. Nothing changes until both
questions are answered.

Then it happens: the destination moves, you are switched to it -- your files
already match it, so nothing on disk changes -- and the review is over. It is
dropped, and both its lines are deleted; everything they held is on the
destination now. So is every review it was copied from, by `transfer` or
`--copy`: those were earlier versions of the same work. Their old tips are
printed, in case. `curate drop` never follows that chain: dropping a copy you
got wrong leaves the review it came from exactly as it was.

## Starting from the wrong base

You often cannot tell until read mode shows you a list that is far too long
or far too short.

```sh
curate drop                  # deletes the clean branch too, unless that would lose a commit
curate start --from <the right one>
```

`drop` deletes the clean branch unless it has commits no other ref has. Once you
have committed something to the clean line and not pushed or branched it
anywhere, `drop` leaves the branch alone and tells you how to delete it by hand.

## Leaving

**You can switch branches straight out of read mode.** Once a review is
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

You can only *look* at one at a time, because there is one working tree, and
`curate review` moves between them:

```sh
curate review feature-2      # pause this review, read feature-2's
curate review feature-1      # and back, approvals where you left them
```

The review you leave is paused, not ended: its approvals, and a fixup paused
on a conflict, wait for you. A branch with no review yet gets one, as `start`
would: `--from <base>`, or on a terminal it asks. With no branch, it asks
which. It refuses, and moves nothing, while your files have anything the
working line you are leaving does not -- an edit, staged or not, or a new
file -- since that would travel onto the other branch and show up as unread
there. Commit it in write mode first.

## Where it keeps things

```
.git/curate/mode, active          which mode, which review
.git/curate/sessions/<clean>/     one per review: the two index files, fixup state,
                                  and the review it was copied from, if it was
.git/curate/log                   every command: when, how long, how much was git
refs/curate/<clean branch>        the approved set, as a real commit, with a reflog
```

When a command feels slow, look at the end of `.git/curate/log`. Each command
is one line there. One that took longer than `CURATE_SLOW` seconds (1 by
default) also lists every git it ran, with when each started and how long it
took. `CURATE_SLOW=0` lists them for everything. Time a list spent waiting for
you to choose is shown apart, and does not count towards slow.

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
