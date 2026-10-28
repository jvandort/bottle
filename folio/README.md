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
  [shpool](https://github.com/shell-pool/shpool) session of their own, started
  the first time you attach to it. Attaching starts the bottle and the session
  if they've stopped. Closing the terminal only detaches it, and whatever is
  running carries on. shpool passes output straight through, so your terminal's
  own scrollback, mouse wheel and selection work; a reattach replays the
  session's last 10000 lines into that scrollback. A session shpool can no
  longer show (an attach that fails at once) is started again, and Claude
  carries on with its conversation. Claude starts with the `claude` feature's
  channel, so events and other bottles' messages reach its session; Claude Code
  asks you to confirm loading it each time it starts. Detach with Ctrl+Space
  Ctrl+Q, or by closing the terminal. tmux is there too, to start in one.
- **Its lane** is two git namespaces in your repo: one the agent pushes into
  and you read, one you push into and it reads. Neither side's push is ever
  refused for what the other did, so conflicts are always the agent's to
  merge. Your repo sees the lane as one remote, `bottle/<thread>`: fetching
  reads the agent's branches, and pushing hands yours over. It works whether
  or not the thread has a bottle right now. A branch you've switched to from it
  follows the agent's pushes, files and all, when nothing of yours is in the
  way (see bottle's README). In the bottle, the agent's lane is
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

## The broker

`folio broker` attaches to the broker: one thread outside your repos, whose
agent talks to your threads' agents for you -- gives them work, follows it up,
relays what comes back -- and keeps a long-term memory of it all, as notes.

```sh
folio broker            # Claude, in the broker (starting it the first time)
folio broker shell      # a shell beside it
folio broker pause      # as folio pause
folio broker notes      # its notes, in Obsidian
```

`folio broker notes` adds the vault to Obsidian's list of vaults the first time
(asking to restart Obsidian if it's running, since Obsidian only reads the list
when it starts), and opens it. Obsidian has no documented way to add a vault,
so this is the undocumented one: its `obsidian.json`.

It's a thread like any other, in a repo folio owns rather than one of yours:
its vault, `~/.folio/repos/broker`, an Obsidian vault of the broker's own
notes, which it organizes as it sees fit. It commits them on `main` and pushes
as it goes, and each push fast-forwards the vault's own `main` and working
tree (bottle's `advance`), so the vault Obsidian opens is always its latest.
Hand it a branch the way you would any thread: push it to `bottle/broker` from
the vault.

Its bottle, `broker`, is made folio's way, not from a repo's spec: `claude` and
`tools`, 2G, and two permissions no thread has: `list-bottles`, to see the
running threads, and `message-any`, to send them messages. Threads can only
answer it. It can't start, stop or pause anything: a paused thread is out of
its reach until you resume it.

## Commands

| Command | |
| --- | --- |
| `folio new THREAD [--from BRANCH]` | Start a thread: its bottle and its lane |
| `folio claude [THREAD]` | Attach to the thread's Claude, starting the thread if it's new |
| `folio shell [THREAD]` | Attach to the thread's shell, starting the thread if it's new |
| `folio pause [THREAD]` | Snapshot a thread, and delete its bottle |
| `folio list` | The repo's threads, most recently active first, and the space a paused one's snapshot takes up |
| `folio delete [THREAD] [--yes]` | Delete a thread: its bottle, its lane and its remote |
| `folio configure set` / `update` / `show` | The repo's spec: its threads' features and memory |
| `folio broker [claude\|shell\|pause\|notes]` | The broker: attach, pause, or open its notes |
| `folio repo list` | Every repo folio has state for |

Leave the thread out, on a terminal, and you choose it from a list: the
repo's threads, most recently active first, with their state. Move with j/k
or the arrow keys, Enter to choose, q or Esc to leave. For `claude` and
`shell` the list starts with a new thread, which asks for its name.

**`folio new`** starts from origin's default branch, or `--from BRANCH`.
`folio claude` and `folio shell` given a thread that doesn't exist start it
as `folio new` would, from origin's default branch, and attach to it.

**`folio delete`** says first what deleting would lose: branches in the lane
that aren't on your branches or origin's, and work in the bottle that never
reached the lane. Then it asks you to type the thread's name. Without a
terminal it needs `--yes`.

**`folio configure`** edits the repo's spec, which every new bottle of its
threads is made from: `set` replaces it, `update` adds features (a feature
given again replaces its options) or changes the memory. Bottle checks the
spec before folio saves it. A repo's spec can't give `permissions`
or a `thread`: folio names each thread's bottle's thread itself, and never
gives it permissions. A repo with no spec gets `tools`, which has the
shpool every thread runs in, and 2G of memory; add `claude` for Claude.

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
