# Durability: getting work out of a bottle

Status: design. Not built. The primitive is tested.

A bottle is one VM with one disk. Everything an agent does lives there until
something copies it out, and today the only thing that copies it out is the
agent choosing to push. That makes durability a property of the agent's
judgement, which is the wrong place for it.

## How work gets lost

Ranked by how little it takes:

1. **Uncommitted, or untracked.** Nothing names it. A `git checkout` that
   discards, a `git stash drop`, an agent tidying up, a crashed VM,
   `bottle delete --force` -- any of these is enough. Most work-in-progress is
   in this state most of the time.
2. **Committed, not pushed.** Safe from ordinary agent mistakes, because a ref
   names it. Lost with the bottle. `bottle delete` and `reset` refuse to destroy
   this without `--force`, though the check has a known false negative (see
   `docs/TODO.md`).
3. **Pushed to the bottle's namespace.** On the host's disk, named by a ref, safe
   from `git gc`, and survives `bottle delete`.
4. **On a branch of yours.** Ordinary work with ordinary backups.

State 1 is both the most common and the least protected, and no amount of
pushing helps, because there is nothing to push. That is the gap.

## The primitive

Build a tree from the working directory in a throwaway index, and commit it
without touching anything the agent is using:

```sh
IDX=.git/snapshot-index                       # not the repo's index
GIT_INDEX_FILE=$IDX git read-tree HEAD
GIT_INDEX_FILE=$IDX git add -A
TREE=$(GIT_INDEX_FILE=$IDX git write-tree)
git commit-tree $TREE -p HEAD -m "snapshot <timestamp>"
```

Tested on a repo mid-work -- modified tracked file, an untracked file,
something staged, and build output covered by `.gitignore`:

- the snapshot contained the tracked edit and the untracked file,
- it excluded the ignored build output,
- the agent's index, working tree and `HEAD` were byte-identical afterwards.

It is invisible. A snapshot can be taken while an agent is mid-edit, or mid
`git add -p`, without the agent observing anything.

This is the same mechanism as a private review index: a second `GIT_INDEX_FILE`
is how git lets a second party read a working tree without owning it.

## Where snapshots go

Not among the branches. Snapshots are not work anyone reviews, and they would
bury the branches that are. `GIT_NAMESPACE` confines everything a bottle pushes,
so any ref path inside it is available:

```
refs/namespaces/bottle-NAME/refs/snapshots/<timestamp>
```

Append-only by timestamp, so no snapshot ever force-updates another and the
"force pushes are deliberate" property is untouched. They accumulate, so
pruning is required rather than optional: keep the last N, thin the rest, drop
any whose tree is identical to its predecessor.

## Who triggers it

The host, not the bottle. A timer inside a bottle is a mechanism the agent can
break with exactly the mistake it exists to protect against; a backup you can
delete by accident is not a backup.

bottled already tracks which bottles are running, and `bottle exec` already
runs a command in one. So bottled runs the snapshot script from outside, and
nothing new goes in the image beyond git, which the base image contract already
requires.

Three triggers, in descending order of value:

1. **Before anything destructive.** `bottle delete`, `bottle reset`,
   `bottle stop`. These are the known-dangerous moments, they need no timer, and
   they cover every deliberate way work dies.
2. **On a timer**, while a bottle is running. Recovery point objective of one
   tick; a hard VM crash loses at most that much.
3. **`bottle snapshot NAME`**, manually, before letting an agent try something
   drastic.

## Cost

A full working-tree stat per snapshot. Keep the snapshot index file between
runs so git's stat cache stays warm, or every tick is a cold scan of the whole
repo. Object growth is mostly deltas against the previous snapshot, which is
cheap, but at one snapshot every few minutes pruning is what keeps it cheap.

Worth measuring before choosing a default interval, on a repo big enough to
hurt.

## What it changes

`bottle delete` can stop refusing. It refuses today because the work would be
gone; once it is not, the refusal is friction:

```
Deleted gradle. Its last snapshot is bottle-gradle/snapshots/2026-09-28T05:00Z,
taken 2 minutes before deletion, including uncommitted changes.
```

The known false negative in the unsaved-work check stops being a data-loss bug
and becomes a wrong sentence in a message.

It also removes a constraint from every other design. Once durability does not
depend on pushing, pushing is free to mean only what it should mean -- "this is
ready for you to look at" -- instead of doubling as the backup, and nobody has
to choose between handing over half-finished work and risking it.

## Minimal version

Triggers 1 and 3 only: snapshot before `stop`, `reset` and `delete`, plus a
manual command. No timer, no pruning to speak of. That covers every deliberate
loss, and the timer can be added later without changing anything else.

## Non-goals

- **Snapshotting the VM's disk.** Opaque, large, and not reviewable with git.
  The value of a snapshot being a commit is that every ordinary tool works on it.
- **Backing up anything outside the repo.** A bottle's home directory, caches
  and installed packages are reproducible from the image and the features.
  `bottle reset` is the recovery path for those.
- **Replacing the agent's own pushes.** Snapshots are for recovery, not review.
  Nothing should read them except a human looking for something they lost.

## Open questions

- What interval, and measured how?
- Should a snapshot be taken when a bottle's egress goes quiet, as a proxy for
  "the agent stopped working", rather than on a fixed clock?
- Should `bottle delete` keep refusing by default even with snapshots, on the
  grounds that a snapshot is a worse artifact than a branch?
