"""curate -- two lines: a working line you write on, a clean line you ship.

One module per seam, and the dependencies run one way:

    errors    Refused, the one exception raised on purpose
    gitcmd    every call made to git, and reading merge-tree's output
    state     what a review is: Session, Repo, the invariant, the versions
    modes     the mode switch -- two renames and a symbolic-ref
    hooks     the three hooks, installing them and handling them
    carry     copying a review onto a rebased branch, and finding its base
    start     beginning one: the base, and naming the clean line
    transfer  copying the review you are in onto the branch that replaced it
    integrate shipping a finished review, adopting others' commits on the way
    fixup     folding into an existing clean commit, and its conflicts
    gitverb   `curate git <command>`: borrow write mode for one command
    commands  status, list, drop
    pick      choosing from a list: the keys, apart from curses drawing it
    ui        asking, when there is a terminal to ask at
    cli       the verbs, and HELP

The one place that could have gone circular is `switch_mode` wanting to offer
`start` when there is no review: modes raises NoReview and cli decides what to
do about it.

This package sits inside a project directory of the same name, so `curate/`
appears twice in its path. That is fine, and `tests/test_launcher.py` keeps it
fine: `bin/curate` puts this directory at the front of `sys.path`, and a
regular package found there wins over the project directory, which would
otherwise be picked up as a namespace package by anything running from the
repository root. The test runs exactly that arrangement.
"""
