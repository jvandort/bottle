# Reviewing a bottle's work

Status: design. Nothing here is built except the stopgap in `hooks/post-receive`.

## Three layers

Keeping these separate is what stops bottle growing a review workflow it has no
business owning.

**bottle** isolates an agent and bridges it to the host. It moves commits in
both directions and knows nothing about what they contain. It does not diff, it
does not review, and it does not read the contents of what it carries -- a rule
enforced by inspecting a commit's text is a rule bottle should not have.

**bottle-workflow** (name pending) is the opinionated layer: the host-side hook
that fires when you commit a review pass, the extra context handed to the agent,
the conventions for how comments are written and answered, and the queue that
stops two agents starting at once. Built alongside bottle, configurable, and off
unless you ask for it.

**A review tool** (name pending) is the human's, not the project's. It has to
work well with the other two, and one design for it is in `../../review/` -- a
standalone tool that knows nothing about bottle, for which an agent's branch is
just one possible source of work to review. Nothing in bottle should depend on
it existing.

The rest of this document is the first layer: what bottle must provide so the
other two are possible.

## The model

A bottle and its host are two peers working on the same repo. Git was built for
exactly that, so the goal is not to invent a review mechanism -- it is to make
the two sides line up well enough that ordinary git collaboration works, and
then get out of the way.

Concretely, the loop:

1. The agent commits and pushes.
2. The work surfaces on the host as a branch.
3. You review it **in your own editor, on your own machine**, indexing locally,
   against commits -- the workflow you already have.
4. You fix what you want to fix, commit it, and say so in the commit message.
5. The agent fetches, sees your commits, rebases its work onto them, and
   resolves whatever conflicts result -- which is a thing agents are good at and
   you should not have to do.
6. You pull and repeat.

Nothing there is bottle-specific except steps 2 and 5. **Review is not bottle's
job**, and neither is diffing: bottle keeps an agent from breaking your machine
and gives it a bridge back. Everything else is git and your editor.

## What already works

The bottle-ward half of the loop exists and is undocumented. A bottle's `host`
remote is unnamespaced for fetch -- only pushes are confined -- so:

```sh
git fetch host          # in the bottle: the host's real local branches, as host/*
git rebase host/work    # rebase onto what the human committed
```

An agent can already see and build on the host's commits. The asymmetry is
deliberate and turns out to be useful: a bottle pushes into a private namespace
the host has to adopt from, but fetches the host's canonical branches, so
`host/<branch>` in a bottle always means "what the human has", never "what I
last pushed".

What is missing is convention, not machinery: nothing tells an agent to fetch
before working, nothing names the branches so the two sides line up, and nothing
tells the agent whose commits it may rewrite.

## What bottle has to provide

**`bottle adopt NAME BRANCH` -- and it should keep the name.** The host's copy
of the agent's `work` should be called `work`, because then the agent's
`git fetch host` produces `host/work`, and `git rebase host/work` closes the
loop with no translation. Adopt is also where commits get re-signed with your
key, since the agent never signs as you, and it must refuse to overwrite an
existing branch.

**A rule the agent follows: rebase your own commits onto the human's, never
rewrite the human's.** Rebasing across a human's commit changes its hash,
detaches any signature, and quietly makes them the agent's. The agent's work
goes on top. This belongs in the bottle's context file (`bottle/bottles.py`,
`context()`), alongside the push instructions.

**Upstream tracking**, so the agent's `git pull --rebase` does the right thing
once a branch exists on both sides.

**`bottle status`** -- what has been pushed, how far ahead of your copy, whether
you have commits the bottle has not seen. Both directions, because the loop has
two.

## Inline comments

Step 4 of the loop covers changes you make yourself. The other half of a review
is the things you want changed but do not want to write: "too wordy", "use the
existing helper", "why?". A pull request would hold those out of band, anchored
to a commit, a path and a line.

Out of band is the wrong choice here. A comment stored against `file.py:42`
goes stale the moment the agent rebases or edits above line 42 -- which is why
PR tools spend so much effort on "outdated" comments. A comment written *in the
file* moves with the code for free, survives every rebase, and needs no
anchoring machinery at all:

```python
# REVIEW: this reads like an agent wrote it; say what it does, not what it is
```

It also matches the gesture. You are already in the file, reading the line. You
type, and it shows up in the diff as an added line, next to the code it is
about, with no path or line number to write down.

The convention that makes it work: the agent removes each marker in the same
commit that addresses it, so a marker still present means still outstanding.

Enforcement is deliberately not bottle's. Refusing to carry a commit because of
what its text contains would make bottle parse the things it transports, and a
bridge that inspects its cargo is the wrong shape. Whatever keeps markers out of
a finished branch -- a lint rule, a pre-commit hook, a habit -- belongs to the
project being worked on or to bottle-workflow.

This whole convention needs nothing from bottle at all.

### Waking the agent

The unsolved half is asynchrony. You commit review comments at eleven at night;
an idle agent never notices, because nothing in a bottle polls.

The trigger belongs on the host, and `bottle exec` already is one:

```sh
bottle exec gradle -- claude -p 'fetch host, rebase onto host/work, address the
  REVIEW comments, remove each one as you go, push'
```

Wrapped, that is **`bottle tell NAME "message"`**: hand a prompt to the bottle's
agent and let it work. The host-to-bottle direction of the bridge, the
counterpart to the agent's push, and the only part of this that is bottle's --
`docs/TODO.md` already sketches the reverse direction under host commands for
the genie.

The `post-commit` hook that fires it, so that committing a review pass *is* the
handoff, belongs to bottle-workflow. So does the thing that stops two agents
running at once, and it should be a **queue rather than a lock**: a review pass
arriving while the agent is busy should wait its turn, not be dropped. What
bottle owes that layer is `bottle exec` and a way to ask whether an agent is
currently running.

## Surfacing the refs

A bottle's pushes land in `refs/namespaces/bottle-NAME/`, which nothing lists:
not `git branch`, not `git branch -r`, not an IDE. So work arrives invisibly,
and step 2 of the loop needs an answer.

**A. Mirror to `refs/heads/bottle-NAME/*`** -- today's stopgap, in
`hooks/post-receive`. Zero configuration and the most natural UI, but it puts
agent work in the host's own branch namespace, which is what the git namespace
exists to prevent, and it has a trap: check out a mirrored branch, commit on it,
and the next agent push force-updates over you. That trap is disqualifying under
the model above, where committing on the agent's branch is the *normal* thing to
do.

**B. A self-fetching remote.** Register each bottle in the repo's config:

```ini
[remote "bottle-NAME"]
    url = .
    fetch = +refs/namespaces/bottle-NAME/refs/heads/*:refs/remotes/bottle-NAME/*
```

`git fetch bottle-NAME` copies the namespace into `refs/remotes/bottle-NAME/*`,
which `git branch -r` lists and IDEs show under Remote Branches, and
`--prune` removes what the bottle deleted. Verified. No hook, nothing written to
`refs/heads`, and remote-tracking refs are the correct category for "someone
else's branches" -- committing on top means creating your own local branch,
which is what you wanted to do anyway.

**C. A remote helper, `git-remote-bottle`.** Makes `bottle::NAME` a URL git and
IDEs understand, fetching live from the bottle rather than from what it last
pushed. The right end state, and B's config stanza can be repointed at it, so B
does not block it.

**D. No standing refs; adopt on demand.** Purest, and unusable without
`bottle status` for discovery.

B is the recommendation: it is the only option that gives the IDE a native,
correctly-categorised view at no cost to the namespace's guarantee, and it makes
"commit on top of the agent's work" a normal local branch rather than a trap.

## Considered and rejected: the editor in the bottle

A remote IDE (Gateway, Remote-SSH) would put you in the same working tree as the
agent, which dissolves the surfacing problem entirely. It was rejected:

- **Indexing belongs on your machine.** A remote backend indexes inside the VM,
  which is the heaviest thing that can run in a bottle. Bottles currently get
  every core and all the Mac's memory, and memory a guest has touched is not
  returned until it stops -- so a bottle holds its indexing high-water mark for
  as long as it runs, times however many bottles are open. That is the binding
  constraint on running several at once, which is the point of bottle.
- **Licensing.** JetBrains activation happens on the remote side, so it means a
  JetBrains credential inside a bottle, against the README's own rule: do not put
  a secret in a bottle you would not hand to the agent. The
  credentials-via-the-proxy idea in `docs/TODO.md` might solve it, but it is an
  unsolved problem today, not a detail.
- **It solves the wrong problem.** Concurrent editing of one tree needs a
  `bottle pause` and an etiquette. Two peers with their own copies needs neither
  -- git already handles two people touching the same code, and the agent is the
  one who should be doing the merging.

## Non-goals

- **Diffing or reviewing.** Not bottle's job. Your editor does this better than
  any command bottle could ship, and a review workflow that requires leaving the
  editor to approve something is worse than either pure alternative.
- **Reading what it carries.** bottle moves commits; it does not inspect their
  contents to enforce anything.
- **Preventing concurrent edits.** Conflicts are a normal outcome, and the agent
  resolves them.

## Sequencing

1. **`bottle status`** -- both directions. Needed by everything else.
2. **`bottle adopt NAME BRANCH`** -- same-name branch, re-signed, never
   overwriting.
3. **The agent's side of the loop** -- context file says to fetch `host` before
   working, to rebase onto the human's commits, and never to rewrite them;
   upstream tracking configured where it can be.
4. **B, the self-fetching remote**, replacing the `post-receive` mirror in the
   same change.
5. **C** when a fetch that does not require the agent to have pushed is worth
   the moving parts.

Durability is a separate concern with its own document: see `durability.md`.
Work should not need to be pushed to be safe, and once it does not, pushing is
free to mean only "this is ready to look at".

## Open questions

- Does an IDE's "Fetch All" over a self-referencing remote do anything
  surprising? (Expected: no, it is a local fetch of a handful of refs.)
- What should `bottle adopt` do when the name is taken -- refuse, suffix, or ask?
- Should the agent merge rather than rebase when the human's branch has moved,
  to keep the human's commits untouched by construction?
