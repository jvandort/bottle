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
  or delete anything it has pushed. You can: `git push bottle/<thread>
  :refs/heads/<branch>` deletes the agent's branch too, if it's still what you
  last fetched of it, and is refused if the agent has pushed to it since.
  `curate integrate` does that for the branch it integrated. The rest stay
  until the thread is deleted.
- **Its agent** is told how to work: every branch it pushes is
  `genie/<feature>-N`, starting with `genie/<thread>-1`, and a rebase or a
  squash is pushed as the next N rather than over the old one.

A thread is **running**; **stopped**, its VM stopped with the disk intact (a
crash, a restart); **paused**, its bottle deleted and a snapshot kept; or
**missing**, its bottle gone with no snapshot, leaving only what's in its
lane. Attaching to a stopped thread starts it, and to a paused one resumes it
in a new bottle. Either way only running processes are lost.

## Notes

Every thread has notes of its own: a repo its agent keeps at `/notes` in its
bottle, for what the work teaches it -- reproducers, research, tools it built,
leads it can't follow yet. The agent commits to `main` and pushes as it goes,
and your copy, `~/.folio/<repo key>/<thread>/notes`, follows it (its `main`
tracks the agent's), so `folio notes THREAD` opens them in Obsidian, current.
They're the thread's for good: a rename takes them along, a delete leaves
them, and a new thread of the same name finds them.

## Pausing

`folio pause` snapshots a thread and deletes its bottle, so a thread you're
not working on holds no VM. The snapshot is the workspace (every branch and
stash, staged and unstaged changes, untracked files, though not ignored ones
like build output) and Claude's conversation and memory. `folio claude` or
`folio shell` resumes it, in a new bottle made from the repo's spec as it is
then, so a resume is also how a thread gets features added since.

## Runners

A thread's bottle can run on another machine of yours, a runner added with
`bottle runner add` (see bottle's README): its sessions, lane, notes and
pause work as here, over ssh. A repo's threads are made on the repo's runner
(`folio configure update --runner desktop`), else here; a thread can have its
own (`folio new fix-it --runner desktop`). It applies whenever the thread's
bottle is made, so moving a thread is pausing it and resuming it where it's
to go: `folio pause fix-it`, then `folio claude fix-it --runner desktop`. Its
snapshot comes back here and goes out again, so it resumes whatever the
runner's CPU. A thread with a bottle can't be given another runner: pause it
first. `folio list` shows where each thread runs, or will.

A drone's Claude gets its credential through this machine, so it can't reach
Claude while this machine is asleep or can't reach the runner.

## Sidecars

A thread can have a sidecar: a second bottle, on a runner of its own (a bigger
machine, say), with a spec of its own, for the jobs that want more than the
thread's bottle has. The thread's agent reaches it as `ssh sidecar` -- a shell,
a command, scp, git over ssh -- and is told so.

```sh
folio sidecar set fix-it --runner desktop --feature jvm:version=21 --memory 32G
folio sidecar update fix-it --memory 64G
folio sidecar show fix-it
folio sidecar remove fix-it
```

It has the thread's repos, fetch-only, as they were when it was made. It's
scratch: it's made just before the thread's bottle, started with it, and
deleted with it, a pause included. So a thread that has a bottle gets its new
sidecar, or loses its old one, when it's paused and resumed. `folio list`
shows each thread's sidecar's runner.

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

It's a thread like any other, but with no repo of yours: its bottle mounts only
its notes, at `/notes`, kept as any thread's are (see Notes), in
`~/.folio/broker-<hash>/broker/notes`. It commits them on `main` and pushes as
it goes, so the vault Obsidian opens is always its latest. Hand it a branch
the way you would any thread's notes: push it to `bottle` from the vault.

A broker from before kept its vault at `~/.folio/repos/broker`, mounted at
`/workspace`. folio moves it -- history, lane and all -- the next time the
broker has no bottle: at `folio broker pause`, or when it starts one.

Its bottle, `broker`, is made folio's way, not from a repo's spec: `claude` and
`tools`, 2G, and two permissions no thread has: `list-bottles`, to see the
running threads, and `message-any`, to send them messages. Threads can only
answer it. It can't start, stop or pause anything: a paused thread is out of
its reach until you resume it.

## Commands

| Command | |
| --- | --- |
| `folio new THREAD [--from BRANCH] [--runner RUNNER]` | Start a thread: its bottle and its lane |
| `folio claude [THREAD] [--runner RUNNER]` | Attach to the thread's Claude, starting the thread if it's new |
| `folio shell [THREAD] [--runner RUNNER]` | Attach to the thread's shell, starting the thread if it's new |
| `folio pause [THREAD]` | Snapshot a thread, and delete its bottle |
| `folio notes [THREAD]` | Open a thread's notes in Obsidian |
| `folio rename THREAD NEW` | Rename a paused thread: its record, lane and remote (its branches keep their names) |
| `folio list` | The repo's threads, most recently active first, where each runs, and the space a paused one's snapshot takes up |
| `folio delete [THREAD] [--yes]` | Delete a thread: its bottle, its lane and its remote |
| `folio configure set` / `update` / `show` | The repo's spec, its threads' features and memory, and its runner |
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
given again replaces its options), changes the memory, or sets `--runner`,
where its threads' bottles are made (see Runners). Bottle checks the spec
before folio saves it: that it's valid, while whether its memory fits is
checked against the runner each bottle is made on. A repo's spec can't give `permissions`
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
