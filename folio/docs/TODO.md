# TODO

The top half is what's left of the initial design (`DESIGN.md`, kept on the
`genie/folio-design-2` branch), copied from it verbatim, with the overview and
whatever's been built taken out. It tracks progress on that design; nothing
new goes in it. The bottom half is a working list: things found along the way,
appended as they come up.

# What's left of the design

## Changes to bottle

### Removed

- **`bottle repo`** and `repos.json`. Specs come from files, repos from
  `--git-repo`.
- **`bottle reset`.** It was delete-then-new with the same arguments, used mostly
  to get a fresh sandbox with new features. A caller with a spec file does that
  with `delete` and `new`, and folio does it with a pause and a resume.
- **The unsaved-work refusal** in `delete`, and its `--force`.
  Bottle discards a sandbox's processes and working tree without asking already;
  deciding what's worth keeping is folio's job. This also retires the weak
  `cat-file -e` check noted in `docs/TODO.md`.
- **Registering the `bottle-<name>` remote** in the user's repo. Folio owns
  remotes now.

`~/BOTTLE.md` itself drops the workflow, which moves to folio: the "Working with
the user", "What can't be rewritten" and "Delivering finished work" sections.
What stays is fact: the sandbox, the proxy, what's installed, and which remotes
it serves and what each one is.

## Threads

### States

| State | Sandbox | The work is in | `folio claude` does |
|---|---|---|---|
| **running** | up | the VM | attach to the running `claude` tmux session |
| **stopped** | VM stopped, disk intact | the VM's disk | start it, start Claude in tmux, attach |
| **paused** | gone | the thread's current slot | new sandbox, restore, start Claude in tmux, attach |

**Stopped** is what's left when something blows up: the terminal, the host, or
the VM. The disk survives, so recovering means reattaching to the same sandbox,
with the features it was created with. Nothing is restored, and only processes
are lost. Folio reads the state live from `bottle status` rather than trusting
`thread.json`.

If a thread's sandbox is missing and it has no snapshot, folio says so and
offers to start a fresh sandbox from the thread's lane: its branches are safe
there, and everything else is gone.

### Pause

1. Stop the thread's tmux server. Its processes get SIGHUP, which also makes
   Claude flush its transcript before the snapshot reads it.
2. Stream `workspace.tar.gz` out over one exec, and `claude.tar.gz` over
   another, into a new slot.
3. Check both archives (`tar -t`), write `thread.json`, and point `current` at
   the slot.
4. `bottle delete`.

If any step before 4 fails, the sandbox is left as it is, stopped or running,
and the thread loses nothing.

### Resume

1. `bottle new --spec` with the repo's `spec.json`, `--git-repo`, the thread's
   remotes, and folio's context.
2. Replace `/workspace` with `workspace.tar.gz`, streamed over stdin.
3. Unpack `claude.tar.gz` into `~/.claude`.
4. Start Claude in tmux and attach.

If `bottle new` fails, for example because the spec no longer validates, the
thread stays paused, its slot untouched, and the error is shown.

Resume runs automatically, as part of `folio claude` or `folio shell` on a
paused thread. The target is a few seconds, image build aside: the VM starts in
about a second, the checkout borrows the mounted objects, and both archives are
small.

### What `workspace.tar.gz` holds

- **`/workspace/.git`**: config, HEAD, the index, every ref, the stash, the
  reflog, and the objects the sandbox made itself. Objects borrowed from the
  mount (through `objects/info/alternates`) aren't copied. The new sandbox mounts
  the same repo at the same path, so the alternates file stays valid.
- **Every tracked and untracked file** that isn't ignored:
  `git ls-files -co --exclude-standard`, skipping tracked files that were
  deleted.

Ignored files (build outputs, `node_modules`) aren't included; they can be
huge, and rebuilding them is cheaper than carrying them.

Restoring this gets back the exact workspace: the branch, staged and unstaged
changes, untracked files, local commits that were never pushed, and stashes. So
pause loses only running processes, and nobody has to be told off for shutting
down a sandbox with unpushed work.

**Risk:** a snapshot relies on objects in the user's repo. If the user deletes
the only ref to a commit the snapshot builds on and the repo is gc'd, restore
fails. Lanes keep everything the agent pushed, and `origin` keeps the upstream,
so this takes deliberate effort. If it happens, the fix is to have the snapshot
repack whatever the mounted repo can't reach.

### What `claude.tar.gz` holds

All of `~/.claude`, minus a denylist:

- **Owned by the image or a feature** (restoring them would pin stale
  settings over a newer image): `settings.json`, `CLAUDE.md`.
- **Caches** (regenerable): `plugins/`, `cache/`, `telemetry/`,
  `policy-limits*`, `remote-settings.json`, `backups/`.

The rest includes the transcripts, subagent transcripts and memory in
`projects/-workspace/`, `/rewind`'s `file-history/`, and prompt history. A
denylist means a new kind of Claude Code state gets captured automatically,
where an allowlist would miss it. `~/.claude.json` is left out for now: the
image seeds it, and nothing in it is needed to resume.

Memory is per thread: each thread's `projects/-workspace/memory/` travels with
its own snapshot.

Checked in a bottle: a fresh home with only what the `claude` feature seeds,
with this archive unpacked over it, resumes with `claude --continue`, and the
conversation is all there.

**Gap:** Claude Code writes some user choices into `settings.json` (`/model`
writes `"model"`), and the denylist drops that file, so a resume forgets them.
Merging the snapshot's `settings.json` keys over the image's is the likely
fix, once it matters.

## CLI

| Command | |
|---|---|
| `folio pause [THREAD]` | Snapshot the thread and delete its sandbox |

**The picker.** Every command that takes a thread, except `new`, opens a picker
when no thread is given: the repo's threads, most recently active first, with
their state. Move with j/k or the arrow keys, Enter to choose. The picker for
`claude` and `shell` also has an entry at the top for a new thread. It's built
on the standard library's `curses`. With no TTY, leaving the thread out is an
error.

**Deleting**, for a paused thread:

- **Work that never reached the lane**: local branches whose commits the lane
  doesn't have, a detached HEAD, uncommitted changes. Folio reads these from the
  live sandbox, or from `workspace.tar.gz` if the thread is paused.

## Advice to the agent

- **Pausing.** A pause keeps the whole workspace and the conversation, but not
  running processes. After a resume, check what was running and start it again.

## Not in scope

- Periodic checkpoints while a thread runs. Adding them later means writing
  the same slots more often.
- `--git-repo` with a URL, and folio on a different machine from its sandboxes.
  The design keeps both possible: no writable mounts, and everything over exec.
- Moving a repo without orphaning its threads.
- tmux UX: the prefix key (Ctrl+B is also Claude Code's background key), mouse
  handling, scrollback, the status bar.
- Curate's own branch bag.
- Migrating existing bottles, lanes or `bottle-*` remotes.

## Order of work

1. **Spike**: done. tmux survives a disconnected exec, and `claude --continue`
   works from a restored `claude.tar.gz`.
2. **Bottle**: `--spec` and `spec validate`, `--git-repo`, `--git-remote`,
   `--checkout`, `--context` and `status` are done. Left: the workflow moved
   out of `~/BOTTLE.md`, and the removals, once folio has replaced them.
3. **Folio, running threads**: done. Repo keys, `configure`, lanes, `new`,
   `claude`, `shell`, `list`, `delete`, with tmux.
4. **Folio, pause and resume**: slots and the stopped state are done; the two
   snapshots and the paused state are next.
5. **The picker.**

# Found along the way

- **A thread's advice is fixed when its bottle is made.** `--context` is
  written once, so a change to `advice.py` reaches only threads created after
  it. A resume makes a new bottle, so it will refresh it there.
- **A spec without `claude` fails tersely.** `folio claude` then gets tmux's
  "can't find session", since the session's command exits at once. Say that
  the repo's spec has no `claude`, and how to add it.
- **`settings.json` choices are lost on resume** (see `claude.tar.gz` above):
  merge the snapshot's keys over the image's.
