# Folio

*Threads of work that outlive their sandboxes.*

[Bottle](../README.md) gives an agent a sandbox. Folio gives it a **thread**:
one line of work in one repo, with a bottle to work in, a lane in the repo for
its commits, and Claude running in the bottle, where closing your terminal
doesn't stop it.

```sh
cd ~/src/foo
folio new fix-flaky-test       # a bottle, and a lane for its commits
folio claude fix-flaky-test    # Claude, in the bottle
                               # ...close the terminal; Claude carries on
folio claude fix-flaky-test    # back to it
folio shell fix-flaky-test     # a shell beside it, in a session of its own
git fetch bottle/fix-flaky-test && git switch genie/fix-flaky-test-1
```

Folio works on the repo you're in. There's nothing to register: the first
folio command in a repo is enough.

## Threads

A thread's name is lowercase words joined by `-`, unique within its repo. Its
bottle is named `<repo key>-<thread>`, so the same thread name in two repos
never clashes. The repo key is the repo's name and a hash of its path.

- **Its bottle** is made from the repo's spec (see `folio configure`), with
  the repo checked out at `/workspace`. In it, Claude and a shell each run in a
  tmux session of their own, started the first time you attach to it.
  Attaching starts the bottle and the session if they've stopped. Closing the
  terminal only detaches it, and whatever is running carries on.
- **Its lane** is two git namespaces in your repo: one the agent pushes into
  and you read, one you push into and it reads. Neither side's push is ever
  refused for what the other did, so conflicts are always the agent's to
  merge. Your repo sees the lane as one remote, `bottle/<thread>`: fetching
  reads the agent's branches, and pushing hands yours over. It works whether
  or not the thread has a bottle right now. In the bottle, the agent's lane is
  its remote `agent` and yours is `user`, read-only. The agent can't rewrite
  or delete anything it has pushed.
- **Its agent** is told how to work: every branch it pushes is
  `genie/<feature>-N`, starting with `genie/<thread>-1`, and a rebase or a
  squash is pushed as the next N rather than over the old one.

A thread is **running**; **stopped**, its VM stopped with the disk intact (a
crash, a restart); **paused**, its bottle deleted and a snapshot kept; or
**missing**, its bottle gone with no snapshot, leaving only what's in its
lane. Attaching to a stopped thread starts it, and to a paused one resumes it
in a new bottle. Either way only running processes are lost.

## Pausing

`folio pause` snapshots a thread and deletes its bottle, so a thread you're
not working on holds no VM. The snapshot is the workspace (every branch and
stash, staged and unstaged changes, untracked files, though not ignored ones
like build output) and Claude's conversation and memory. `folio claude` or
`folio shell` resumes it, in a new bottle made from the repo's spec as it is
then, so a resume is also how a thread gets features added since.

## Commands

| Command | |
| --- | --- |
| `folio new THREAD [--from BRANCH]` | Start a thread: its bottle and its lane |
| `folio claude THREAD` | Attach to the thread's Claude |
| `folio shell THREAD` | Attach to the thread's shell |
| `folio pause THREAD` | Snapshot a thread, and delete its bottle |
| `folio list` | The repo's threads, most recently active first, and the space a paused one's snapshot takes up |
| `folio delete THREAD [--yes]` | Delete a thread: its bottle, its lane and its remote |
| `folio configure set` / `update` / `show` | The repo's spec: its threads' features and memory |
| `folio repo list` | Every repo folio has state for |

**`folio new`** starts from origin's default branch, or `--from BRANCH`.

**`folio delete`** says first what deleting would lose: branches in the lane
that aren't on your branches or origin's, and work in the bottle that never
reached the lane. Then it asks you to type the thread's name. Without a
terminal it needs `--yes`.

**`folio configure`** edits the repo's spec, which every new bottle of its
threads is made from: `set` replaces it, `update` adds features (a feature
given again replaces its options) or changes the memory. Bottle checks the
spec before folio saves it. A repo with no spec gets `tools`, which has the
tmux every thread runs in; add `claude` for Claude.

```sh
folio configure set --feature tools --feature claude --feature jvm:version=21 --memory 8G
```

## Development

```sh
cd folio && python3 -m unittest discover -s tests -t .
```

The tests run `bin/folio` against `tests/fake_bottle.py`, a stand-in for
bottle's CLI that keeps its bottles in a JSON file and runs commands in a real
clone of the repo, so they need no VM.
