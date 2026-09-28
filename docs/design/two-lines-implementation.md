# Two lines: implementation guide

Companion to `two-lines.md`, which has the model and the rationale. This has the
parts you need to actually write it: a glossary, the exact plumbing, the state
on disk, the algorithms, and the edge cases that will bite.

## Language

**Python 3, standard library only.**

Every operation is a subprocess call to git plumbing; there is no numerical
work, no concurrency, and no need for a git library. What the tool actually does
is parse command output and run a small state machine, which is what Python is
good at. It ships as a single file, and anyone running this alongside bottle
already has Python 3.11+.

Do not use a git binding (pygit2, GitPython). The whole design rests on precise
control of index files, `GIT_INDEX_FILE`, and `merge-tree` semantics; a binding
puts a layer between you and exactly the things you care about, and adds a
compiled dependency for no benefit.

Rewrite in Go only if it is ever distributed to people who do not already have
Python. Nothing in the design would change -- it would still shell out to git.

## Packaging it

Git's extension mechanism is that any executable named `git-<name>` on `PATH`
becomes `git <name>`, with `GIT_DIR` and friends already set. There is no
in-process plugin API -- every extension point git has (merge tools, diff tools,
credential helpers, remote helpers, clean/smudge filters, textconv, hooks) is
"run this program", which is why a single Python file is a first-class citizen
here and a library binding is not.

So the tool can be invoked as `git <name>` for free. Mind the naming:
`git-review` is already the OpenStack project's Gerrit client, and `git-absorb`,
`git-branchless` and `git-machete` are all taken. Pick something else before
the muscle memory sets.

## Glossary

Everything below is a git concept the implementation depends on. If any of these
are fuzzy, the code will be too.

### Objects

**Object database.** The content-addressed store under `.git/objects`. Every
object is identified by the hash of its contents, called an **oid** (object id) --
the 40-character hex string you see everywhere. Objects are immutable: you never
change one, you write a new one.

**Blob.** A file's contents. Just bytes, with no name and no permissions.

**Tree.** A directory: a sorted list of (mode, oid, name) entries, where each
entry points at a blob (a file) or another tree (a subdirectory). A tree is a
complete snapshot of a directory hierarchy. **This is the central object in this
design** -- "what I have approved" is a tree.

**Commit.** A pointer to one tree, plus zero or more parent commits, plus author,
committer and message. A commit does not contain a diff; it contains a whole
snapshot. Diffs are computed between commits on demand, which is why rewriting
history is cheap and why approval-by-content is robust.

**tree-ish.** Anything git can resolve to a tree: a tree oid, a commit (means its
tree), a branch name, `HEAD`, `HEAD~3`, `main:src/`. Most commands that want a
tree accept any of these.

### Refs

**Ref.** A named pointer to an oid, stored under `.git/refs/` or in
`.git/packed-refs`. `refs/heads/main` is a branch, `refs/tags/v1` a tag,
`refs/remotes/origin/main` a remote-tracking ref. Refs are the only mutable part
of git; everything else is immutable objects.

**Branch.** A ref under `refs/heads/` that moves forward when you commit on it.
There is nothing else to a branch.

**HEAD.** What you have checked out. Normally a *symbolic ref* -- a pointer to a
branch name (`ref: refs/heads/main`) -- so committing moves that branch.
**Detached HEAD** means it holds an oid directly, so committing moves nothing.
This design switches modes by repointing HEAD between two branches.

**Reflog.** A per-ref log of every value it has held, in
`.git/logs/refs/...`. This is what makes a force-update recoverable and a
deletion not (a deleted ref takes its reflog with it).

**Merge base.** The best common ancestor of two commits: where they diverged.
`git merge-base A B`. A three-way merge needs one.

**Fast-forward.** Moving a ref to a descendant of its current value -- no merge
needed, because the old value is already an ancestor of the new one.

### The index

**Index** (also *staging area*, also *cache* -- all the same thing). A binary
file, by default `.git/index`, holding a flat list of (path, mode, oid, stat
data) entries. It is a **tree in a convenient mutable form**: `git write-tree`
turns it into a real tree object, `git read-tree` loads one into it.

Three things make it central here:

- **It is per index file, not per repository.** `GIT_INDEX_FILE` points git at a
  different one, and a linked worktree gets its own. So "a second staging area"
  is not exotic -- it is one environment variable.
- **It holds a stat cache**: size and mtime for each path, so `git status` can
  skip reading files that have not changed. This is why swapping index files is
  fast and why you want to keep the same file around rather than rebuilding it.
- **It has slots for conflicts.** During a merge a path can have up to three
  entries, called **stages**: stage 1 is the merge base's version, stage 2 is
  "ours", stage 3 is "theirs". Stage 0 means resolved.

**Working tree.** The actual files on disk. In this design it holds the working
line's content and is the one thing that must not move when you switch modes.

### Merging

**Three-way merge.** Given a base, ours and theirs, produce a result by taking
each side's change relative to the base. A **conflict** is when both sides
changed the same region differently.

**`git merge-tree --write-tree`** (git 2.38+) does a three-way merge entirely in
the object database, writing the result tree and reporting conflicts, **without
any working tree**. This is what makes `fixup` possible without checking
anything out. See the output format below.

**`git replay`** (git 2.44+) rebases without a worktree. Not used here -- it has
no autosquash and we do the surgery directly -- but it is git's own
acknowledgement that rebasing should not require checked-out files.

**rerere** ("reuse recorded resolution"). Git's cache of how you resolved a
given conflict, keyed by the conflict's content. Not used initially; the obvious
future optimisation.

### Plumbing versus porcelain

**Porcelain** commands are for humans (`git status`, `git commit`, `git rebase`):
their output is not a stable interface. **Plumbing** commands are for scripts
(`git write-tree`, `git commit-tree`, `git merge-tree`, `git update-index`):
their output is stable and documented. This tool is almost entirely plumbing.

## The plumbing it uses

| command | role here |
| --- | --- |
| `git write-tree` | turn the current index into a tree object |
| `git read-tree <tree-ish>` | load a tree into an index (discards stat cache) |
| `git update-index --cacheinfo <mode>,<oid>,<path>` | set one index entry, no file needed |
| `git update-index --refresh` | re-stat the working tree to rebuild the stat cache |
| `git hash-object -w --stdin` | write a blob, get its oid |
| `git cat-file blob <oid>` | read a blob out |
| `git commit-tree <tree> -p <parent> -m <msg>` | make a commit, touching nothing else |
| `git merge-tree --write-tree --merge-base=<c> <a> <b>` | three-way merge in the object database |
| `git rev-list --reverse <a>..<b>` | the commits to replay, oldest first |
| `git symbolic-ref HEAD refs/heads/<branch>` | switch which branch HEAD names |
| `git update-ref <ref> <oid>` | move a branch |
| `git reset --mixed <commit>` | set index from a commit, leave the working tree |
| `git for-each-ref` / `git rev-parse` | resolve names to oids |

Two porcelain commands appear deliberately: `git add -p` (the editor usually
does this instead) and `git commit` in review mode (the editor's commit button,
which is correct because HEAD is the clean line).

## State on disk

```
.git/review/
  clean               clean line's ref name, e.g. refs/heads/clean
  working             working line's ref name
  mode                "write" or "review"
  index.write         the write mode index, when not live
  index.review        the review mode index, when not live
  fixup/              present only while a fixup is in progress
    target            oid of the commit being fixed up
    approved          oid of the approved tree, captured when the fixup started
    todo              one oid per line, commits still to replay, oldest first
    done              oid of the rebuilt clean tip so far
    conflicted_tree   oid of the tree merge-tree produced, with markers
    stages/           <path>.BASE / .LOCAL / .REMOTE scratch files
    resolved          "<path>\\t<oid>" lines, resolutions for the current step
```

Exactly one of `index.write` / `index.review` exists at a time; the other one is
live as `.git/index`.

## Algorithms

### `start --from <base>`

```
working  := current branch          (fail if HEAD is detached)
base     := --from, defaulting to merge-base(working, working@{upstream})
git branch <clean> <base>
mkdir .git/review; write clean, working, mode=write
GIT_INDEX_FILE=.git/review/index.review git read-tree <base>
GIT_INDEX_FILE=.git/review/index.review git update-index --refresh   # ignore exit
```

`--refresh` exits non-zero when entries genuinely differ from the working tree.
That is the normal case here -- it means there is work to review -- so ignore it.

### Mode switch

```
from := read(mode); if from == to: return
take .git/index.lock exclusively                 # so no concurrent git races us
rename .git/index              -> .git/review/index.<from>
rename .git/review/index.<to>  -> .git/index
git symbolic-ref HEAD refs/heads/<branch for to>
release .git/index.lock
git update-index --refresh -q                    # ignore exit
write(mode, to)
```

Renames rather than copies: atomic, and free regardless of repository size.
Taking `.git/index.lock` matters because an editor runs git constantly in the
background; that is the lock git itself uses, so holding it makes the swap safe.

The working tree is not read or written, so the stat cache in each index stays
valid and `git status` is fast immediately after a switch.

### `fixup <target>`

Precondition: mode is review, the clean line is linear, `target` is an ancestor
of the clean tip.

```
C := clean tip
T := git write-tree                        # the approved tree
if target == C:                            # degenerate case: amend
    new := commit-tree T -p C^ (metadata of C); update-ref clean new; return
todo := git rev-list --reverse target..C
F := commit-tree T -p C -m "fixup"         # the approval, as a commit

# step 0: fold the approval into the target
tree := merge(base=C, ours=target, theirs=F)
if conflict: save state; report; exit
new := commit-tree tree -p target^ (metadata of target)

# steps 1..n: replay the rest of the clean line onto it
for Y in todo:
    tree := merge(base=Y^, ours=Y, theirs=new)
    if conflict: save state; report; exit
    new := commit-tree tree -p new (metadata of Y)

git update-ref <clean> new
git reset --mixed <clean>                  # index := new tip's tree, working tree untouched
```

The final `reset --mixed` matters only when a conflict was resolved, because
then the rebuilt tip's tree is no longer `T`. It is cheap and always correct, so
do it unconditionally.

"Metadata of X" means preserving the original author name, email and date, and
the message. Pass them to `commit-tree` as `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`
and `GIT_AUTHOR_DATE` in the environment; read them with
`git log -1 --format='%an%n%ae%n%aI%n%B' <commit>`. Forgetting this silently
rewrites authorship, which is the kind of bug nobody notices for a month.

### Reading `merge-tree`'s output

Success: exit 0, one line, the result tree's oid.

Conflict: exit 1, and:

```
<oid of the tree, with conflict markers in the conflicted files>
<mode> <oid> <stage>\t<path>        # repeated, stages 1, 2, 3
<blank line>
<informational messages>
```

So parse line 1 as the tree, then `\t`-separated lines until the blank, into
`{path: {stage: oid}}`. Use `-z` if paths may contain odd characters, which
changes the separators but not the structure. Exit codes above 1 are real
errors, not conflicts.

Both representations are usable: the tree already has ordinary conflict markers
in it, and the stages give a merge tool its three inputs.

### `resolve`

```
tool := git config merge.tool
cmd  := git config mergetool.<tool>.cmd
for path, stages in conflicts:
    write stages/<path>.BASE   from stage 1
    write stages/<path>.LOCAL  from stage 2
    write stages/<path>.REMOTE from stage 3
    run cmd with $BASE $LOCAL $REMOTE $MERGED substituted
    if $MERGED was written: oid := hash-object -w $MERGED; append to resolved
```

A missing stage means the file was added on one side only; handle it as an empty
input rather than crashing.

### `fixup --continue`

```
read state
GIT_INDEX_FILE=<tmp> git read-tree <conflicted_tree>
for path, oid in resolved:
    GIT_INDEX_FILE=<tmp> git update-index --cacheinfo 100644,<oid>,<path>
tree := GIT_INDEX_FILE=<tmp> git write-tree
... continue the loop from where it stopped ...
```

### `fixup --abort`

`rm -rf .git/review/fixup`. There is genuinely nothing else to undo: no ref was
moved and no file was written.

### `wip`

From write mode, `git add -A` then `git commit`. From review mode, the same
thing without disturbing anything:

```
GIT_INDEX_FILE=<tmp> git read-tree <working line tip>
GIT_INDEX_FILE=<tmp> git add -A            # respects .gitignore
tree := GIT_INDEX_FILE=<tmp> git write-tree
c    := git commit-tree <tree> -p <working line tip> -m "wip"
git update-ref <working> <c>
```

HEAD, the review index and the working tree are all untouched. This is the
snapshot primitive from `durability.md`, pointed at a branch instead of a
parking ref.

### Keeping up with a working line someone else writes

There is no command. Switch to write mode and use git. If the tree is dirty,
refuse, and offer `--commit-first` to make a `wip` commit on the working line --
never an autostash. See `two-lines.md`.

## Edge cases

- **Root commits.** `target^` does not exist if `target` is the first commit;
  `commit-tree` with no `-p` is correct.
- **A non-linear clean line.** Merges in the clean line break the replay. Refuse
  at `start` and at `fixup`, rather than producing something subtly wrong.
- **`target` not an ancestor of the clean tip.** Refuse.
- **Empty approval.** If `write-tree` equals the clean tip's tree there is
  nothing to fix up; say so and exit 0.
- **A replay step that becomes empty** (the fixup already contained that commit's
  whole change) -- keep it as an empty commit or drop it; dropping is friendlier,
  but say which happened.
- **Submodules and symlinks.** `merge-tree` handles them, `--cacheinfo` needs
  the right mode (`160000`, `120000`). Do not hardcode `100644`.
- **Binary files.** `merge-tree` reports them as conflicts with no useful
  markers. The stages still work for a merge tool that handles binaries; a text
  merge tool will not.
- **Case-insensitive filesystems.** macOS default. Two paths differing only in
  case in a tree will misbehave. Rare; document rather than solve.
- **An editor holding the index.** Covered by taking `.git/index.lock`, but an
  editor may also cache what it last read. If a mode switch does not show up,
  that is the cause, and it is the one thing in this design not yet verified
  against a real IDE.

## Testing

Everything is deterministic plumbing over temporary repositories, so the tests
write no fixtures and need no network. Build histories with `commit-tree` and
`update-index --cacheinfo` directly rather than by checking files out -- it is
faster, exact, and keeps the tests honest about what the tool actually
manipulates.

The cases worth covering first, all of which were exercised by hand while
designing this:

- Approving hunks into a private index leaves the repository's own index
  untouched.
- A mode switch changes neither the files nor their mtimes.
- The review index survives the working line being committed to, amended and
  rebased, and only genuinely new content comes back as unapproved.
- A fixup with no conflict is absorbed, not appended, and never touches the
  working tree.
- A fixup that conflicts at the fold conflicts again on replay, both resolve,
  and the result is correct.
- `--abort` after a conflict leaves every ref exactly where it was.
