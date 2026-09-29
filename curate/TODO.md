# TODO

Things noticed while using it, not built yet. Not in priority order.

## Living alongside git

The rule this section keeps coming back to: **round one is IDE-first, and the
git tools you already use should be convenient from inside a review.** Not new
tools that duplicate them.

- **Keeping up with the working line is three commands.** When someone pushes
  to the branch you are reviewing, catching up is `curate write`, `git pull`,
  `curate review`. This is tedious. A `curate git <command>` that drops to
  write mode, runs it and comes back would be very convenient, and would cover
  `log`, `fetch` and `rebase` in one idea rather than a verb each.

- **Say why git refused, when the reason is curate.** `git pull` in review mode
  gives you `There is no tracking information for the current branch`, which is
  true, useless, and says nothing about review mode -- HEAD is the clean line,
  which has no upstream and never will. A `post-checkout`-style hook cannot
  intercept this, but `curate status` could notice the clean line has no
  upstream and say so.

## Packaging

- **`git curate`.** Git turns any `git-<name>` on `PATH` into a subcommand, and
  `git-curate` is free. Nothing creates that link yet; `bin/curate` is the
  plain one.

- **A branch named `curate` is currently fatal.** `start` refuses outright
  while one exists, because refs are paths and it rules out the whole
  `curate/*` namespace. That is one rule rather than an escape hatch, which is
  the right trade now. If it ever bites someone real, the fallback is to allow
  `--clean` to override it, or to put clean lines somewhere that cannot
  collide with a branch name at all -- `refs/curate/heads/*` rather than
  `refs/heads/curate/*`, at the cost of them not showing up in `git branch`.

## Conflicts

- **Reuse recorded resolutions.** The working line keeps moving, so the same
  fold gets retried and the same conflict resolved again. Resolutions are keyed
  by (base, ours, theirs), which is exactly what `git rerere` already caches.
  Deferred until the repetition was felt; it now has been.

- **`resolve` with no merge tool configured** writes the three sides and the
  marked-up file into `.git/curate/sessions/<clean>/fixup/stages/` and says
  where, and `--continue` now reads a hand edit back out of them. Opening
  `$EDITOR` on the marked-up file would still save a step.

- **Binary files in a fixup conflict.** `merge-tree` reports them as conflicts
  with no useful markers. The three stages are written as bytes, so a merge
  tool that handles binaries works; a text one will not. `--continue` asks
  only that you changed the file, since there are no markers to remove, which
  catches walking away but not resolving it badly.

## Rough edges

Known, and mostly deliberate. Here rather than in the README because they are
things to fix or decide, not things to learn.

- **Files the working line added show as unversioned** in review mode, since
  the clean line does not know them. Staging fixes the display.
- **Updating a working line someone else writes does move files.** Unavoidable,
  because the working line moved, and the only operation that does.

## Resuming

- **A review cannot be restarted onto a clean line that already exists.**
  `start` refuses when the branch is there, which is right for a typo and
  wrong for picking a review back up after `drop`, or after state was damaged
  badly enough to throw away. There is no way back to a clean line that has
  commits on it, which is exactly the one worth keeping. `--clean` naming an
  existing branch, with the base read from it rather than asked for, is
  probably the shape.

## Smaller

- **Migrations, once `STATE_VERSION` ever moves.** A curate that finds a higher
  state version refuses, which is the safe half. Nothing upgrades a lower one
  yet, and the answer may simply be to rebuild: the index files are a cache and
  the approved set is in a ref, so a migration would have very little to carry.
