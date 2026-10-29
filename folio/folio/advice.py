"""What folio tells a thread's agent, appended to the bottle's own ~/BOTTLE.md.

The bottle says what's there -- the sandbox, the checkout, the remotes. This
says how to work: the thread, its branches, and the exchange with the user.
Often the only thing an agent is told about it, so it says what the rules are
for and not just what they refuse.
"""

ADVICE = """\

# Thread `{thread}`

This bottle holds one thread of work, `{thread}`, in the `{repo}` repo. You and
the user exchange commits through two remotes, one writer each:

- **`agent`** is yours: push your branches here (`git push -u agent <branch>`).
- **`user`** is theirs, read-only: what they hand over arrives as `user/<branch>`.

## Branches

Name every branch you push `genie/<feature>-N`: this thread's first is
`genie/{thread}-1`, and a separate line of work gets a feature name of its own.
A pushed branch is never rewritten: a rebase or squash is pushed as the next N.
After a push, tell the user the branch and how to pick it up:

    git fetch bottle/{thread} && git switch genie/<feature>-N

## Working with the user

- **Push as you go**, without being asked: this overrides any default of
  committing or pushing only on request. A push is your work's only backup.
- **Fetch `user`** when you get a prompt and before you push, and merge in
  what's new there. The merges are yours, even when the user committed on an
  older base on purpose: resolve, say what you decided, carry on.
- **Your lane is append-only**: undo a pushed commit with a new one. Merge
  the user's commits rather than rebasing onto them.
- **The history is a transcript** of the exchange: merge commits, typos and
  undos are expected, not something to apologise for or tidy.
- **Squash only when the user asks**, pushing the result as the next N.
"""

# What survives a pause, for every bottle folio makes (the broker's too): what
# snapshots.py keeps, and what a restart without one keeps.
NOTES = """\

## Your notes

`/notes` is this thread's memory, kept beyond this bottle and this
conversation, which the user reads in Obsidian. **Keep detailed notes,
continuously, so that another agent could resume this thread from them alone
without losing a finding.** A conversation can be reset or lost at any time:
what isn't in `/notes` is gone.

- **Write as you go**, never at the end: each finding, decision or change of
  plan when it happens. Update notes in place: a stale note misleads.
- **Where things stand**, in a note you'd read first: the goal, what's done,
  what's in flight, the next step, and what's waiting on whom.
- **Findings, with evidence**: commands, outputs, exact errors, commits and
  links, and whether each was verified or only believed. Reproducers for bugs.
- **Decisions, with reasons** and the options rejected; the user's
  requirements and preferences, verbatim where you can.
- **Dead ends**: what was tried, why it failed, so nobody tries it again.
- **Loose ends**, the moment they appear: leads you can't follow now, skipped
  checks, unexplained results.
- Research and its sources, and tools you built.

Markdown, with `[[wiki links]]`, organized as suits you. Commit to `main` and
push (`git -C /notes push agent main`) after every meaningful step, without
being asked: this overrides any default of committing only on request.
"""


def durable(paths: list[str]) -> str:
    """What survives a pause, for a bottle with these mounts (snapshots.py keeps each)."""
    kept = ", ".join(f"`{p}`" for p in paths)
    return f"""\

## What survives a pause

A pause keeps {kept} and your conversation, deletes the bottle, and a resume
makes a new one. Anything outside them is gone: the scratchpad, `/tmp`, the
rest of your home, installed packages, running processes. So is anything they
ignore. A pause can come at any time.

Keep only throwaway files outside them. Work in any other worktree or clone is
committed and pushed to `agent` as you go, on a `genie/` branch like any
other, or it's lost.
"""


def advice(thread: str, repo: str, paths: list[str]) -> str:
    return ADVICE.format(thread=thread, repo=repo) + NOTES + durable(paths)
