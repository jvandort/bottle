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


SIDECAR = """\

## Your sidecar

`ssh sidecar` reaches this thread's sidecar: a bottle of its own on {where},
with {memory} of memory and features {features}, for the jobs that want more
than this bottle has: builds, test suites, anything long or big. It's you
there, as `genie`, in a shell or with a command; scp and git over ssh work too.

- It has this bottle's repos at the same paths, as they were when it was made,
  with the same remotes: a push from there lands in this thread's lane, as one
  from here does. Two checkouts writing one lane share it as two people share a
  remote: a push refused as not a fast-forward lost a race, so fetch `agent`,
  merge, and push again. Work there on a branch of its own (the next
  `genie/<feature>-N`), so that the two rarely race.
- `/notes` there is a second checkout of these notes: commit there only what
  you find there, then `git pull agent main` before you push, there and here.
- Your work gets there over ssh, committed or not:
  `git push sidecar:/workspace $(git stash create || git rev-parse HEAD):refs/heads/job`,
  then `ssh sidecar 'cd /workspace && git checkout -q --detach job && ...'`.
- It's scratch: a pause deletes it and keeps nothing of it, and a resume makes
  a new one. Push what's worth keeping before then, from there or from here.
"""


def sidecar(runner: str, spec: dict) -> str:
    """What a thread's agent is told about its sidecar."""
    features = ", ".join(f"`{f}`" for f in spec.get("features", [])) or "none"
    return SIDECAR.format(where=f"runner `{runner}`", memory=spec["memory"], features=features)


SIDECAR_ITSELF = """\

# Thread `{thread}`'s sidecar

This bottle is the sidecar of thread `{thread}`, in the `{repo}` repo: its agent
runs in the thread's own bottle and reaches this one over ssh, for jobs that
want more room. Its repos are as they were when it was made, with the thread's
remotes: a push from here lands in the thread's lane, which the thread's bottle
pushes into too, so fetch and merge before you push, and a push refused as not
a fast-forward lost a race: fetch, merge, push again. It's scratch: a pause of
the thread deletes it and keeps nothing of it, so push what's worth keeping.
"""


def sidecar_itself(thread: str, repo: str) -> str:
    """What a sidecar says about itself, to whoever is in it."""
    return SIDECAR_ITSELF.format(thread=thread, repo=repo)


def advice(thread: str, repo: str, paths: list[str], sidecar_: str = "") -> str:
    return ADVICE.format(thread=thread, repo=repo) + NOTES + durable(paths) + sidecar_
