# Contributing

## Running the tests

```sh
python3 -m unittest discover -s tests -t . -v            # all of them
python3 -m unittest tests.test_git_assumptions -v        # the git ones alone
python3 -m unittest tests.test_fixup -v                  # one file's worth
```

That runs the `curate` sitting beside the tests. 

To test a copy somewhere else, run:

```sh
CURATE_BIN=/path/to/curate python3 -m unittest discover -s tests -t .
```

A relative path is resolved against your current directory before the tool is
run, because it runs with `cwd` set to a throwaway repository and would
otherwise be looked for in there. If no tool is found
at all, the tests that drive it skip and only `GitAssumptions` runs -- see its
docstring for why those exist.

One file per area, sharing `tests/support.py`: the throwaway-repository harness,
`CurateTestCase`, and how the tool under test is found.

The pty tests need a terminal, because what they test is gated on `isatty`.
They fork one, kill the child in a `finally`, and cap at twenty seconds, so a
hang fails rather than wedging the suite.

## Conventions

Histories are built with `commit-tree` and `update-index --cacheinfo` rather
than by checking files out, so each test says exactly what it means and never
depends on what happens to be in the working tree.

**Approving is not a verb.** The editor stages, so the tests stage: `git add`
for a whole file, a crafted blob for one hunk. If the tool ever needs its own
approve command, the model has gone wrong.

Several tests assert on `mtimes()` rather than on file contents. That is
deliberate: "the working tree was restored afterwards" and "the working tree
was never touched" are different claims, and the second one is the design.

## How it is built

**Python 3.11+, standard library only, fully typed.** Every operation is a
subprocess call to git plumbing; there is no numerical work, no concurrency, and
no need for a git library.

```sh
python3 -m mypy --strict .        # from this directory: package and tests
```

`--strict`, so an unannotated function is an error rather than a silently
untyped one. The floor is 3.11, which buys `X | None` in annotations without
`from __future__ import annotations`, and builtin generics (`dict[str, str]`)
rather than `typing` imports.

`bin/curate` is the command, a thin launcher exactly like `bin/bottle`: check
the interpreter, put the package on `sys.path`, call `main`. There is no second
entry point inside this directory -- one command, one file that starts it.

The package is `curate/curate/`, so the name appears twice in the path. See its
`__init__.py` for why that is safe and what keeps it safe.

One module per seam, listed in `curate/__init__.py`, which is where that list
lives so it is next to the modules it describes.

The dependencies run one way, and `curate/__init__.py` says which way and what
keeps them there.

Do not use a git binding (pygit2, GitPython). The whole design rests on precise
control of index files, `GIT_INDEX_FILE` and `merge-tree` semantics; a binding
puts a layer between you and exactly the things you care about, and adds a
compiled dependency for no benefit.

The commands it leans on, and why: `write-tree` and `read-tree` move trees in
and out of an index, `update-index --cacheinfo` sets one entry with no file on
disk, `merge-tree --write-tree` does a three-way merge entirely in the object
database (which is what lets `fixup` work without a checkout), `commit-tree`
makes a commit and touches nothing else, and `symbolic-ref` is the whole of a
mode switch. Two porcelain commands appear deliberately: `git add -p`, which
the editor usually does instead, and `git commit` in review mode, which is the
editor's commit button and is correct because HEAD is the clean line.

## Versions

`VERSION` is the tool. `STATE_VERSION` is the layout under `.git/curate`,
stamped in `.git/curate/version`. Bump it when that layout changes
incompatibly: a curate that finds a higher number refuses rather than reading
state it half understands. `curate --version` prints both.
