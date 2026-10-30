"""Beginning a review: choosing the base, and naming the clean line."""

import uuid
from dataclasses import dataclass

from .branches import fetch, local_for, switch_to, to_review
from .carry import Carried, carry, choose_base, find, report
from .errors import Refused
from .gitcmd import Objects, git, run
from .hooks import install
from .pick import choose
from .state import CLEAN_PREFIX, Repo, Session, encode, named, short, write
from .ui import ask_for_base, interactive, uninterrupted


def branch_conflict(name: str, objects: Objects) -> str | None:
    """A branch that cannot coexist with `name`, because refs are paths.

    refs/heads/curate and refs/heads/curate/anything cannot both exist: one is
    a file where the other needs a directory. So a branch called exactly
    `curate` rules out the whole namespace, and it is worth saying that rather
    than relaying git's "cannot lock ref".
    """
    parts = name.split("/")
    for i in range(1, len(parts)):
        prefix = "/".join(parts[:i])
        if objects.rev(f"refs/heads/{prefix}") is not None:
            return prefix
    under = run(["for-each-ref", "--count=1", "--format=%(refname:short)",
                 f"refs/heads/{name}/"])
    return under.stdout.strip() or None


def cmd_start(repo: Repo, argv: list[str]) -> int:
    current = repo.session()
    if current and repo.mode == "review" and repo.consistent():
        raise Refused("finish the current review first: `curate write`")
    if not argv and interactive():
        return start_chosen(repo)

    base_arg = clean_arg = copy_arg = None
    rest = []
    i = 0
    while i < len(argv):
        if argv[i] == "--from":
            i += 1
            base_arg = argv[i] if i < len(argv) else None
        elif argv[i] == "--clean":
            i += 1
            clean_arg = argv[i] if i < len(argv) else None
        elif argv[i] == "--copy":
            i += 1
            copy_arg = argv[i] if i < len(argv) else None
            if copy_arg is None:
                raise Refused("--copy needs the review to copy: its branch")
        else:
            rest.append(argv[i])
        i += 1
    if rest:
        raise Refused(f"unexpected argument: {rest[0]}")

    head = repo.head
    if not head:
        raise Refused("HEAD is detached. The working line has to be a branch.")
    source = find(repo, copy_arg) if copy_arg else None
    begin(repo, prepare(repo, head, base_arg=base_arg, clean_arg=clean_arg,
                        source=source))
    return 0


def start_chosen(repo: Repo) -> int:
    """Ask which branch to review, out of everything there is, then begin.

    The branch you are on is the one offered, but it is not taken silently:
    `curate read` on a branch that merely happens to be checked out is as
    likely to be a mistake as a decision. Fetching first, because the branch
    worth reviewing is often the one somebody just pushed.
    """
    fetch(None)
    rows = to_review(repo)
    if not rows:
        raise Refused("no branches to review")
    head = repo.head
    cursor = next((i for i, r in enumerate(rows) if r.value == head), 0)
    chosen = choose(["Review which branch?", ""], rows, cursor=cursor)
    local, tip = local_for(chosen, repo.objects)

    if local != head and any(s.working == local for s in repo.sessions()):
        # Nothing to start, but somewhere to go: this is how you get back to it.
        switch_to(local, chosen, tip, repo.objects)
        repo.recover()
        print(f"curate: {named(local)} has a review already; switched to it.")
        return 0

    # Every question before anything moves, so cancelling the base leaves you
    # where you were, and so does anything prepare refuses.
    clean_line_for(repo, local)
    base = ask_for_base(tip, repo.objects, repo.clean_lines())
    beginning = prepare(repo, local, base_arg=base)
    with uninterrupted():
        if local != head:
            switch_to(local, chosen, tip, repo.objects)
        begin(repo, beginning)
    return 0


@dataclass
class Beginning:
    """A review that can begin: every check passed, and nothing written but
    objects, so refusing here or anywhere before leaves nothing behind."""
    session: Session
    working: str                     # refs/heads/<name>
    base: str
    source: Session | None
    carried: Carried

    @property
    def clean_name(self) -> str:
        return short(self.session.clean)


def clean_line_for(repo: Repo, working: str, clean_arg: str | None = None) -> str:
    """The name a new review's clean line gets, refused if it can't: checked
    before any question, so nobody answers one for nothing."""
    working_name = short(working)
    if repo.objects.rev(f"refs/heads/{CLEAN_PREFIX}") is not None:
        # Refs are paths, so this branch rules out the whole namespace. Curate
        # could dodge it with a different name, but then the rule people have
        # to remember stops being one rule.
        raise Refused(
            f"there is a branch named `{CLEAN_PREFIX}`, and curate keeps every "
            f"clean line under `{CLEAN_PREFIX}/`. Refs are paths, so the two "
            f"cannot coexist -- if you use curate, you cannot have a branch with that name.\n"
            f"Rename it: `git branch -m {CLEAN_PREFIX} <something-else>`.")

    clean_name = clean_arg or f"{CLEAN_PREFIX}/{working_name}"
    blocker = branch_conflict(clean_name, repo.objects)
    if blocker:
        raise Refused(f"'{clean_name}' collides with the branch '{blocker}': "
                      f"refs are paths, so git cannot have both")

    if Session(repo, encode(clean_name)).exists:
        raise Refused(f"a review of '{clean_name}' already exists. "
                      f"`curate status`, or `curate drop {clean_name}`.")
    clean = repo.objects.rev(f"refs/heads/{clean_name}")
    if clean is not None and not clean_name.startswith(f"{CLEAN_PREFIX}/"):
        raise Refused(f"branch {clean_name} already exists")
    if clean is not None:
        # In curate's namespace, and no review has it, or the check above
        # would have said so: what's left of one whose record is gone.
        mine = repo.objects.rev(working)
        if mine is None:
            what = ""
        elif git("rev-list", "-n1", clean, "--not", mine, check=False) == "":
            what = f", with nothing {named(working)} doesn't have"
        else:
            what = f", with commits of its own: `git log {working_name}..{clean_name}`"
        raise Refused(f"branch {clean_name} already exists, but no review has it: it's left over "
                      f"from one whose record is gone{what}. If you don't need it, "
                      f"`git branch -D {clean_name}`, and start again.")

    return clean_name


def prepare(repo: Repo, working: str, base_arg: str | None = None,
            clean_arg: str | None = None,
            source: Session | None = None) -> Beginning:
    """Everything `start` checks and works out, for a working line that need
    not be checked out -- nor exist yet, when `transfer` is about to make it."""
    clean_name = clean_line_for(repo, working, clean_arg)
    session = Session(repo, encode(clean_name))

    if source and source.working == working:
        raise Refused(f"that is the review of {named(working)}, the branch you are on")

    if base_arg:
        base = repo.objects.rev(base_arg)
        if base is None:
            raise Refused(f"cannot resolve {base_arg}")
    elif source:
        base = choose_base(source, working)
    elif interactive():
        # Asking beats guessing: --from is the one argument that decides what
        # the review contains, so a person present gets to see and confirm it.
        base = ask_for_base(working, repo.objects, repo.clean_lines())
    else:
        # And with nobody to ask, the flag, as every command takes one: a
        # guess nobody saw is a review of the wrong commits.
        raise Refused("where does the clean line start? `curate start --from <base>`")

    # Copying writes objects and no refs, so a copy that is refused part way
    # leaves nothing behind, exactly like a start that is.
    carried = (carry(repo, source, base) if source else
               Carried(base, git("rev-parse", f"{base}^{{tree}}")))
    session.clean, session.working = f"refs/heads/{clean_name}", working
    return Beginning(session, working, base, source, carried)


def begin(repo: Repo, b: Beginning, opening: bool = False) -> None:
    """Write what `prepare` worked out: the clean line, and the review.
    `opening`: the caller goes on to open it in read mode, so says so itself."""
    session, source, carried = b.session, b.source, b.carried
    with uninterrupted():
        git("branch", b.clean_name, carried.tip)
        repo.stamp()                       # before any other state exists
        session.id = uuid.uuid4().hex
        write(session.dir / "id", session.id)
        write(session.dir / "clean", session.clean)
        write(session.dir / "working", b.working)
        write(session.dir / "base", b.base)
        if source:
            write(session.dir / "copied-from", source.id)

        idx = session.index("review")
        repo.index_of(carried.approved, idx)
        run(["update-index", "--refresh"], index=idx)   # non-zero means work to review
        repo.set_state("write", session)
        session.record("write")
    install(repo)

    working_name = short(b.working)
    if source:
        report(source, carried)
    print(f"curate: clean line '{b.clean_name}' at {carried.tip[:8]}, "
          f"working line '{working_name}'.")
    if not opening:
        print(f"        `curate read` to start reading; `curate write` to go back.")
    if source:
        print(f"        Wrong base? `curate drop {working_name}` undoes the copy.")
        print(f"        The review of {named(source.working)} is untouched, and "
              f"ends when this one is integrated.")
    else:
        print(f"        Wrong base? `curate drop {working_name}` undoes all of this, "
              f"while nothing is approved.")
