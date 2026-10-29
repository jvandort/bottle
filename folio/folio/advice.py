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
DURABLE = """\

## What survives a pause

A pause keeps `/workspace` and your conversation, deletes the bottle, and a
resume makes a new one. Anything outside `/workspace` is gone: the scratchpad,
`/tmp`, the rest of your home, installed packages, running processes. So is
anything `/workspace` ignores. A pause can come at any time.

Keep only throwaway files outside `/workspace`. Work in any other worktree or
clone is committed and pushed as you go, or it's lost.
"""


def advice(thread: str, repo: str) -> str:
    return ADVICE.format(thread=thread, repo=repo) + DURABLE
