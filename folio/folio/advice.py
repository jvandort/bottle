"""What folio tells a thread's agent, appended to the bottle's own ~/BOTTLE.md.

The bottle says what's there -- the sandbox, the checkout, the remotes. This
says how to work: the thread, its branches, and the exchange with the user.
Often the only thing an agent is told about it, so it says what the rules are
for and not just what they refuse.
"""

ADVICE = """\

# Thread `{thread}`

This bottle holds one thread of work, `{thread}`, in the `{repo}` repo. You and
the user exchange commits through two remotes, a lane each:

- **`agent`** is yours: push your branches here (`git push -u agent <branch>`).
- **`user`** is the user's, and read-only: what they hand over arrives here,
  as `user/<branch>`. There's nothing in it until they push.

One writer each, so neither side's push is ever refused for what the other
did. When both of you have moved a branch, it is your responsibility to merge.

## Branches

Every branch you push should be named `genie/<feature>-N`. This thread's
default feature is `{thread}`, so its first branch should be
`genie/{thread}-1`: create it before you commit, from what's checked out unless
you're told otherwise. A separate line
of work in this thread gets a feature name of its own, starting at 1.

A pushed branch is never rewritten. To rebase or squash one, push the result as
the same feature with the next N, and leave the old one as it is.

Once you've pushed a branch, tell the user what it's called and how to pick it
up:

    git fetch bottle/{thread} && git switch genie/<feature>-N

## Working with the user

You and the user both work on the thread's branches, from separate repos and
sharing no disk. The two lanes are a conversation, and the history they leave
is the record of it. That record is not intended to be tidy, but rather a
transcript of the collaboration between you and the user.

- **Fetch when you're given a prompt** (`git fetch user`), and again before
  you push, in case something arrived while you were working. If
  `user/<branch>` has commits yours doesn't, merge them in.
- **Push as you go, not when you're finished.** Every push is also the only
  backup this work has.
- **The merges are yours.** The user may commit from a base older than your
  last push, deliberately: it hands you the merge. Resolve it, say what you
  decided, and carry on.
- **Messy history is expected here.** Merge commits, typos in messages, commits
  that undo earlier ones -- that's the shape of a real exchange, not something
  to apologise for or offer to tidy up.

## What can't be rewritten

- **Your lane is append-only**: force-pushes and deletes are refused. To undo
  anything you've pushed -- a bad commit, a stray file, something the user asks
  you to remove -- commit the undo on top.
- **The user's commits are theirs.** Your work goes on top of them. Once you've
  pushed, merge their commits rather than rebasing onto them; rebase only what
  you haven't handed over yet.

## Delivering finished work

When the user explicitly asks for it -- never on your own initiative -- squash
the work into a clean history and push it as the next N of its feature. The
branch it came from stays exactly as it is.
"""


def advice(thread: str, repo: str) -> str:
    return ADVICE.format(thread=thread, repo=repo)
