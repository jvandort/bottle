# Running the tests

The tool does not exist yet. These tests were written first, from the design in
`README.md` and `IMPLEMENTATION.md`, and an implementation is finished when they
pass.

```sh
python3 -m unittest test_review -v                 # 8 run, 43 skipped
REVIEW_BIN=./review python3 -m unittest test_review -v   # all 51
```

Without `REVIEW_BIN` the tests that drive the tool skip, and only
`GitAssumptions` runs. Those eight do not exercise the tool at all: they pin the
behaviours of git that the whole design rests on, and they pass today. If one of
them ever fails, the design is wrong rather than the implementation, which is
why they are in the same file and run by default.

| class | what it fixes in place |
| --- | --- |
| `GitAssumptions` | a second index file is invisible to the first; hunk staging honours it; `merge-tree` merges and conflicts without a working tree, atomically; `commit-tree` loses authorship unless told; review refs collide on slashes; `post-index-change` fires when an editor stages; rebase refuses a dirty tree |
| `Start` | the clean line begins at the base; refuses a detached HEAD, and a second start |
| `ModeSwitch` | HEAD follows the mode; **no file's mtime changes**; unstaged in review mode is the to-do list; each mode keeps its own staged set |
| `Approving` | committing in review mode extends the clean line with the approved content only; junk never gets in; nothing auto-approves, including your own edits |
| `WorkingLineMoves` | approvals survive commits, amends and rewritten history; a re-touched file comes back; the clean line does not move |
| `Fixup` | absorbed rather than appended; never touches the working tree; preserves authorship; tip amend, root commit, and the three refusals |
| `FixupConflicts` | reports without moving anything; names the later commit that will conflict too; `resolve` offers all three stages; `--abort` restores every ref; **you can keep working while one is paused** |
| `Sessions` | two branches keep separate reviews; list and drop; slashes in branch names; an index file is only a cache |
| `Guardrails` | git protects an unfinished review and stops protecting a finished one; being moved out is detected; the tool refuses to clobber; approvals are recorded as they are made |

## Conventions

Histories are built with `commit-tree` and `update-index --cacheinfo` rather
than by checking files out, so each test says exactly what it means and never
depends on what happens to be in the working tree.

**Approving is not a verb.** The editor stages, so the tests stage: `git add`
for a whole file, a crafted blob for one hunk. If the tool ever needs its own
approve command, the model has gone wrong.

Several tests assert on `mtimes()` rather than on file contents. That is
deliberate: "the working tree was restored afterwards" and "the working tree was
never touched" are different claims, and the second one is the design.
