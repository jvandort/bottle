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

If a thread's sandbox is missing and it has no snapshot, folio says so and
offers to start a fresh sandbox from the thread's lane: its branches are safe
there, and everything else is gone.

## CLI

**The picker.** Every command that takes a thread, except `new`, opens a picker
when no thread is given: the repo's threads, most recently active first, with
their state. Move with j/k or the arrow keys, Enter to choose. The picker for
`claude` and `shell` also has an entry at the top for a new thread. It's built
on the standard library's `curses`. With no TTY, leaving the thread out is an
error.

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
4. **Folio, pause and resume**: done.
5. **The picker.**

# Found along the way

- **A spec without `claude` fails tersely.** `folio claude` then gets tmux's
  "can't find session", since the session's command exits at once. Say that
  the repo's spec has no `claude`, and how to add it.
- **A resumed workspace keeps its old remotes.** `.git/config` comes back from
  the snapshot, so a thread paused before its remotes changed (`work`, before
  `agent` and `user`) resumes with the old ones, and the new bottle serves
  the new ones. Re-add the bottle's remotes after a restore.
- **Bottle's own hints show through folio.** `bottle new`'s stdout reaches the
  terminal, so a resume ends with bottle's "Open a shell with: bottle shell
  <bottle>", the wrong advice inside folio. Silencing stdout would also hide a
  login's URL (`claude setup-token` prints it there), so it wants a quiet mode
  in bottle that keeps a login's output.
- **Make `bottle new` and `delete` faster.** Timed from bottle.log, with the
  image built: `new` 4.0s, `delete` 2.1s, `pause` 1.6s before its delete. The
  anchor is about 1.5s of each `new` and `delete`; that's bottle's
  (docs/TODO.md, Network). The rest:
  - **~0.3s per `container exec`.** `new` makes three in setting up (contract,
    CA, workspace); `pause` five (list-panes, kill-server, two archives,
    work.txt). Fold them where the output allows.
  - **The workspace's first fetch** (part of 1.2s with the CA and egress) has
    the host `git fetch origin` before serving origin. A new bottle could
    serve what the repo has and leave fetching to the agent. Measure first.
