"""Beginning a review: choosing the base, and naming the clean line."""

from .errors import Refused
from .gitcmd import git, rev, run
from .hooks import install
from .state import CLEAN_PREFIX, Repo, Session, encode, short, write
from .ui import ask_for_base, interactive


def branch_conflict(name: str) -> str | None:
    """A branch that cannot coexist with `name`, because refs are paths.

    refs/heads/curate and refs/heads/curate/anything cannot both exist: one is
    a file where the other needs a directory. So a branch called exactly
    `curate` rules out the whole namespace, and it is worth saying that rather
    than relaying git's "cannot lock ref".
    """
    parts = name.split("/")
    for i in range(1, len(parts)):
        prefix = "/".join(parts[:i])
        if rev(f"refs/heads/{prefix}") is not None:
            return prefix
    under = run(["for-each-ref", "--count=1", "--format=%(refname:short)",
                 f"refs/heads/{name}/"])
    return under.stdout.strip() or None


def cmd_start(repo: Repo, argv: list[str]) -> int:
    base_arg = clean_arg = None
    rest = []
    i = 0
    while i < len(argv):
        if argv[i] == "--from":
            i += 1
            base_arg = argv[i] if i < len(argv) else None
        elif argv[i] == "--clean":
            i += 1
            clean_arg = argv[i] if i < len(argv) else None
        else:
            rest.append(argv[i])
        i += 1
    if rest:
        raise Refused(f"unexpected argument: {rest[0]}")

    head = repo.head
    if not head:
        raise Refused("HEAD is detached. The working line has to be a branch.")
    working_name = short(head)

    current = repo.session()
    if current and repo.mode == "review" and repo.consistent():
        raise Refused("finish the current review first: `curate write`")

    if rev(f"refs/heads/{CLEAN_PREFIX}") is not None:
        # Refs are paths, so this branch rules out the whole namespace. Curate
        # could dodge it with a different name, but then the rule people have
        # to remember stops being one rule.
        raise Refused(
            f"there is a branch named `{CLEAN_PREFIX}`, and curate keeps every "
            f"clean line under `{CLEAN_PREFIX}/`. Refs are paths, so the two "
            f"cannot coexist -- if you use curate, you cannot have a branch with that name.\n"
            f"Rename it: `git branch -m {CLEAN_PREFIX} <something-else>`.")

    clean_name = clean_arg or f"{CLEAN_PREFIX}/{working_name}"
    blocker = branch_conflict(clean_name)
    if blocker:
        raise Refused(f"'{clean_name}' collides with the branch '{blocker}': "
                      f"refs are paths, so git cannot have both")

    session = Session(repo, encode(clean_name))
    if session.exists:
        raise Refused(f"a review of '{clean_name}' already exists. "
                      f"`curate status`, or `curate drop {clean_name}`.")
    if rev(f"refs/heads/{clean_name}") is not None:
        raise Refused(f"branch {clean_name} already exists")

    if base_arg:
        base = rev(base_arg)
        if base is None:
            raise Refused(f"cannot resolve {base_arg}")
    elif interactive():
        # Asking beats guessing: --from is the one argument that decides what
        # the review contains, so a person present gets to see and confirm it.
        base = ask_for_base(head)
    else:
        upstream = rev(f"{working_name}@{{upstream}}")
        if upstream is None:
            raise Refused("no upstream to infer a base from; pass --from <base>")
        base = git("merge-base", head, upstream)

    git("branch", clean_name, base)
    repo.stamp()                       # before any other state exists
    write(session.dir / "clean", f"refs/heads/{clean_name}")
    write(session.dir / "working", head)
    write(session.dir / "base", base)
    session.clean, session.working = f"refs/heads/{clean_name}", head

    idx = session.index("review")
    idx.unlink(missing_ok=True)
    git("read-tree", base, index=idx)
    run(["update-index", "--refresh"], index=idx)   # non-zero means work to review
    repo.set_state("write", session)
    session.record("write")
    install(repo)

    print(f"curate: clean line '{clean_name}' at {base[:8]}, "
          f"working line '{working_name}'.")
    print(f"        `curate review` to start reading; `curate write` to go back.")
    print(f"        Wrong base? `curate drop {working_name}` undoes all of this, "
          f"while nothing is approved.")
    return 0
