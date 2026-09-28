# Reviewing a bottle's work

Status: design. Nothing here is built except the stopgap in `hooks/post-receive`.
Written to get the decision out of a chat log; the recommendation at the end is
a proposal, not a commitment.

## The loop

An agent works in a bottle; you decide what to keep. The full loop is:

1. the agent pushes,
2. you find out there's something to look at,
3. you read it,
4. you give feedback, or fix it yourself,
5. the agent revises,
6. you see **what changed since you last looked**.

bottle currently supports step 1. Steps 2 and 3 work only because of a
stopgap, step 4 is impossible without opening a shell, and step 6 has nothing
at all. Most of the difficulty is that these want different things: step 3
wants a rich diff, step 4 wants a writable file, step 6 wants durable state.

## Where the work lives now

A bottle's pushes land in a git namespace, `refs/namespaces/bottle-NAME/`,
because that's what keeps the host's own refs out of reach (see `bottle/host.py`).
Nothing lists a namespaced ref: not `git branch`, not `git branch -r`, not an
IDE. So work arrives invisibly.

`hooks/post-receive` mirrors each pushed branch to `refs/heads/bottle-NAME/*`
so there is something to look at. That is a stopgap and should not outlive this
document: it puts agent work back in the host's own branch namespace, which is
exactly what the git namespace exists to prevent. It also has a trap -- check
out a mirrored branch, commit on it, and the next agent push silently
force-updates over you.

## Surfacing the refs

**A. Mirror to `refs/heads/bottle-NAME/*`** -- today. Zero configuration, IDEs
show a branch folder, and it is the most natural UI of any option. Costs: agent
work in your branch namespace, `git branch` clutter, and the clobber trap above.

**B. A self-fetching remote.** Register each bottle in the repo's config:

```ini
[remote "bottle-NAME"]
    url = .
    fetch = +refs/namespaces/bottle-NAME/refs/heads/*:refs/remotes/bottle-NAME/*
```

`git fetch bottle-NAME` then copies the namespace into `refs/remotes/bottle-NAME/*`,
which `git branch -r` lists and IDEs show under Remote Branches, and
`git fetch --prune bottle-NAME` removes what the bottle deleted. Verified.
No hook, no daemon involvement, nothing written to `refs/heads`. The oddity is a
remote whose URL is the repo itself, and "Fetch All" in an IDE will hit it
(cheap, local, no network).

**C. A remote helper, `git-remote-bottle`.** Makes `bottle::NAME` a URL that
git and IDEs understand, fetching live from the bottle. Conceptually the right
model -- a bottle *is* a remote -- and it also fixes the case where work exists
only inside a bottle nobody pushed from, because fetch becomes host-initiated.
Costs: a helper on `PATH`, starting a stopped bottle to serve a fetch, and more
moving parts than B. B's config stanza can be repointed at it later, so B does
not block C.

**D. No standing refs; `bottle adopt NAME BRANCH` on demand.** Purest: work
becomes visible when you decide to take it, which is also where commit
re-signing belongs. Unusable on its own -- you cannot see what you have not
adopted -- so it needs discovery (`bottle status`) to be a real option.

## Where you edit

Reviewing is not only reading. A wording change is faster to make than to
describe, and describing it precisely enough for an agent to reproduce is worse
than doing it yourself. So "can I edit these files" is a first-class
requirement, not a nicety.

**Remote IDE (editor in the bottle).** You edit the real files in place. The
agent's branches are ordinary `refs/heads/*` there, so the surfacing problem
above disappears entirely -- the namespace stops being a display and becomes a
drop-box that only `bottle adopt` reads. Groundwork is already listed in
`docs/TODO.md` under Remote IDEs and SSH.

**Adopt, edit, push back in.** You adopt the branch, edit locally with all your
own tooling, commit, and bottle pushes your commits into the bottle
(`docs/TODO.md`: sending work in). Fully git-native, no concurrency problem,
nothing extra enters the bottle. The cost is a round-trip per change, and the
agent has to rebase onto you -- which agents are good at, and which they can
resolve conflicts for.

**A filesystem mount.** sshd is already in the base image, so SSHFS from the Mac
would expose the bottle's tree to a local IDE. Not recommended: FUSE on current
macOS is painful and IDE indexing over it is worse.

### Licensing

JetBrains Gateway runs the IDE backend on the remote side, and activation
happens there, so using it means a JetBrains credential inside a bottle. The
README's rule is explicit: do not put a secret in a bottle you would not hand
to the agent directly. Egress allows any public destination, so "inside a
bottle" means "can leave".

Three ways out:

- Use an editor whose remote mode needs no licence in the bottle (VS Code
  Remote-SSH, Zed). Cheapest, but not the editor in question.
- Accept it deliberately, on the grounds that the bottle is your own machine's
  VM and the threat model is mistakes rather than hostility -- but write that
  down rather than discover it during setup.
- Deliver it through the egress proxy. `docs/TODO.md` already proposes injecting
  credentials as request headers at the proxy so they never enter a bottle. If
  activation is an ordinary HTTPS call, this is the same mechanism and would
  solve it properly.

### Memory and CPU

An IDE backend indexing a large repo is the heaviest thing anyone would put in
a bottle, and bottles currently get every core and all of the Mac's memory.
Memory a guest has touched is not returned to the host until the bottle stops,
so a bottle holds its indexing high-water mark for as long as it runs. With
several bottles open at once -- the case that makes bottle worth having -- this
is the binding constraint.

That promotes two `docs/TODO.md` entries from nice-to-have to prerequisites:

- **Resource limits** -- configurable CPUs and memory per bottle, and a default
  lower than "everything".
- **Persistent cache volumes for IDE servers**, so indexing is not paid again
  on every `bottle reset`.

Reviewing on the host has none of this cost, which is the strongest argument
for B or C surviving alongside a remote IDE rather than being deleted.

### Concurrency

You and the agent writing the same tree at the same time is a real problem, not
a theoretical one. Minimum viable answer: **`bottle pause NAME`** -- stop the
agent, not the VM -- so editing is not a race. Cheap, and it makes in-bottle
editing usable at all.

The alternative shape, which needs no new mechanism: keep editing on the host,
commit on top, push in, and let the agent rebase. Agents are good at rebasing
and at resolving the conflicts that result. That turns concurrency from a
correctness problem into a merge, which git already knows how to do.

## Safety: pushing is the backup

Review models change how often work gets pushed, and pushing is currently the
only thing that gets work out of a single VM. Ranked by how easily work is lost:

1. **Uncommitted in the bottle.** Nothing references it. A crashed VM, a bad
   `git checkout`, an agent's `git stash drop`, `bottle delete --force` -- gone.
2. **Committed, unpushed.** Safe from most agent mistakes, lost with the bottle.
   `bottle delete` and `reset` refuse to lose this without `--force`, but that
   check has a known gap (see `docs/TODO.md`: it uses `git cat-file -e`, which
   is satisfied by an unreachable object, so a branch pushed and then deleted on
   the host reads as saved).
3. **Pushed to the namespace.** On the host's disk, named by a ref, safe from
   `git gc`, and survives `bottle delete`.
4. **Adopted to a branch.** Ordinary work.

Two conclusions follow.

**Durability must not depend on the agent remembering to push.** Today it
entirely does. A periodic push from inside, or the host-initiated pull that a
remote helper (C) would enable, would decouple "work is safe" from "the agent
decided it was ready".

**Reviewing in the bottle must not become the reason work stays there.** If the
review surface is the bottle's own working tree, the natural rhythm is to review
before pushing -- which reviews the least durable copy. Whatever we build, the
push should happen first and the review should read what was pushed, or the
push should be automatic enough that the distinction stops mattering.

This is also the argument against treating "keep it unstaged until reviewed" as
a safety-neutral habit. It is not: state 1 is the most dangerous state there is,
and a review workflow that parks work there is trading durability for
bookkeeping. See `review-state.md` -- storing review state in a ref instead of
the index means approved and unapproved work can both be committed and pushed.

## Recommendation

Sequenced by what is useful soonest and regretted least:

1. **`bottle status`** -- has anything been pushed, how far ahead, merged or
   not. Needed by every option. Small.
2. **`bottle adopt NAME BRANCH`** -- namespaced ref to your own branch,
   re-signed with your key, never overwriting an existing branch. The hand-back,
   under every model.
3. **`bottle review`** (see `review-state.md`) -- durable, hunk-level review
   state that the agent's git cannot destroy. Independent of where you edit.
4. **B, the self-fetching remote** -- native IDE visibility for the cost of a
   config stanza, and it survives even if remote IDEs win, because reviewing on
   the host is cheap and reviewing in a bottle is not. Delete the
   `post-receive` mirror when this lands.
5. **`bottle pause`**, then the remote IDE groundwork already in the TODO.

C stays the end state for a fetch that does not require the agent to have
pushed. D is what B degrades to if the config stanza proves more trouble than
it is worth.

## Open questions

- Does an IDE's "Fetch All" over a self-referencing remote do anything
  surprising? (Expected: no, it is a local fetch of a handful of refs.)
- Can a JetBrains licence be delivered at the egress proxy rather than inside
  the bottle?
- What is the right default for how often a bottle pushes, if pushing becomes
  automatic?
- Should `bottle adopt` be the only way work reaches `refs/heads/*`, or should
  it be one of several?
