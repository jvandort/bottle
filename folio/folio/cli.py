"""What you can type, and what happens when you do."""

import argparse
import sys
import time

from . import engine, lanes, picker, repos, threads
from .errors import FolioError


PICK = "choose one from a list"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="folio", description="Threads of work in bottles: each one a bottle, a lane for its commits, and Claude.",
    )
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")

    new = commands.add_parser(
        "new", help="start a thread",
        description="Start a thread in the repo you're in: a bottle for it, and a lane for its commits, "
        "fetched from the remote bottle/THREAD. Then `folio claude THREAD` or `folio shell THREAD` to work in it.",
    )
    new.add_argument("thread", metavar="THREAD", help="its name: lowercase words joined by '-', e.g. fix-flaky-test")
    new.add_argument("--from", dest="base", metavar="BRANCH",
                     help="the branch to start from (default: origin's default branch)")
    new.set_defaults(run=_new)

    for name, session, what in (("claude", threads.CLAUDE, "Claude"), ("shell", threads.SHELL, "shell")):
        attach = commands.add_parser(
            name, help=f"attach to a thread's {what}",
            description=f"Attach to the thread's {what}, starting its bottle and the session if they've stopped, "
            "or starting the thread itself, as `folio new` does, if there's none of that name. "
            "Closing the terminal only detaches: what's running in it carries on.",
        )
        attach.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
        attach.set_defaults(run=_attach, session=session)

    pause = commands.add_parser(
        "pause", help="snapshot a thread and delete its bottle",
        description="Snapshot the thread -- its workspace, every branch and stash, uncommitted changes, and "
        "Claude's conversation -- and delete its bottle. Only running processes are lost. `folio claude` or "
        "`folio shell` resumes it, in a new bottle made from the repo's spec as it is then.",
    )
    pause.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
    pause.set_defaults(run=_pause)

    list_ = commands.add_parser("list", help="the repo's threads", description="The repo's threads, most recently active first.")
    list_.set_defaults(run=_list)

    delete = commands.add_parser(
        "delete", help="delete a thread",
        description="Delete a thread: its bottle, its lane and the bottle/THREAD remote, and what folio knows "
        "about it. Says first what that would lose, and asks you to type the thread's name.",
    )
    delete.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
    delete.add_argument("--yes", action="store_true", help="don't ask (needed without a terminal)")
    delete.set_defaults(run=_delete)

    configure = commands.add_parser("configure", help="the repo's spec: its threads' features and memory")
    configure_commands = configure.add_subparsers(dest="configure_command", required=True, metavar="COMMAND")
    feature_help = "a feature, optionally with options, e.g. jvm:version=21 (repeatable)"
    memory_help = f"memory for each bottle, in M or G, e.g. 8G (default: {repos.DEFAULT_MEMORY})"
    for name, help_ in (("set", "replace the spec: exactly these features, and this memory"),
                        ("update", "change the spec: add features (one given again replaces its options), or the memory")):
        command = configure_commands.add_parser(
            name, help=help_, description=f"{help_[0].upper()}{help_[1:]}. Bottle checks it before it's saved. "
            "Threads get it in their next bottle; a bottle already running keeps what it has.",
        )
        command.add_argument("--feature", action="append", default=[], metavar="FEATURE", help=feature_help)
        command.add_argument("--memory", help=memory_help)
        command.set_defaults(run=_configure, replace=name == "set", parser=command)
    show = configure_commands.add_parser("show", help="the repo's spec")
    show.set_defaults(run=_configure_show)

    repo = commands.add_parser("repo", help="the repos folio knows")
    repo_commands = repo.add_subparsers(dest="repo_command", required=True, metavar="COMMAND")
    repo_list = repo_commands.add_parser("list", help="every repo folio has state for, wherever you are")
    repo_list.set_defaults(run=_repo_list)

    args = parser.parse_args(argv)
    try:
        return args.run(args)
    except FolioError as e:
        print(f"folio: error: {e}", file=sys.stderr)
        return 1


def _new(args: argparse.Namespace) -> int:
    thread = _start(repos.current(), args.thread, args.base)
    print(f"Work in it with: folio claude {thread.name}  (or folio shell {thread.name})")
    return 0


def _start(repo: repos.Repo, name: str, base: str | None = None) -> threads.Thread:
    print(f"Starting thread {name} in {repo.path}...", file=sys.stderr)
    thread = threads.create(repo, name, base)
    # Flushed: attaching next replaces this process, and with it anything still buffered.
    print(f"Started thread {thread.name} in bottle {thread.bottle}. Its branches: git fetch bottle/{thread.name}",
          flush=True)
    return thread


def _thread(args: argparse.Namespace, repo: repos.Repo, new: bool = False) -> threads.Thread | None:
    """The thread the command names (started, if it's new and the command may start one), else the one
    chosen in the picker; None if none was."""
    if args.thread is not None:
        if new and not threads.exists(repo, args.thread):
            return _start(repo, args.thread)
        return threads.load(repo, args.thread)
    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        raise FolioError(f"give the thread: `folio {args.command} THREAD` (choosing one needs a terminal)")
    all_ = threads.all_(repo)
    if not all_ and not new:
        raise FolioError(f"{repo.path} has no threads; start one with `folio new THREAD`")
    states = threads.states(repo, all_)
    width = max((len(t.name) for t in all_), default=0)
    choices = [picker.NEW] if new else []
    choices += [picker.Choice(f"{t.name:<{width}}  {states[t.name]['state']:<8}  {_ago(t.last_active)}", t.name)
                for t in all_]
    choice = picker.pick(f"folio {args.command}: choose a thread (j/k, Enter; q to leave)", choices)
    if choice is None:
        return None
    if choice.thread is not None:
        return threads.load(repo, choice.thread)
    try:
        name = input("The new thread's name: ").strip()
    except (EOFError, KeyboardInterrupt):
        print()
        return None
    return _start(repo, name) if name else None


def _attach(args: argparse.Namespace) -> int:
    repo = repos.current()
    thread = _thread(args, repo, new=True)
    if thread is None:
        return 0
    threads.attach(repo, thread, args.session)


def _pause(args: argparse.Namespace) -> int:
    repo = repos.current()
    thread = _thread(args, repo)
    if thread is None:
        return 0
    print(f"Pausing thread {thread.name}...", file=sys.stderr)
    threads.pause(repo, thread)
    print(f"Paused thread {thread.name}. Resume it with: folio claude {thread.name}")
    return 0


def _list(args: argparse.Namespace) -> int:
    repo = repos.current()
    all_ = threads.all_(repo)
    states = threads.states(repo, all_)
    rows = [("THREAD", "STATE", "SNAPSHOT", "ACTIVE", "FEATURES", "BRANCHES")]
    for thread in all_:
        row = states[thread.name]
        size = threads.snapshot_size(repo, thread) if row["state"] == "paused" else None
        rows.append((thread.name, row["state"], _size(size) if size is not None else "-", _ago(thread.last_active),
                     " ".join(row.get("features", ())) or "-",
                     " ".join(sorted(lanes.branches(repo.path, thread.name))) or "-"))
    _table(rows)
    return 0


def _delete(args: argparse.Namespace) -> int:
    repo = repos.current()
    thread = _thread(args, repo)
    if thread is None:
        return 0
    if not args.yes:
        if not sys.stdin.isatty():
            raise FolioError(f"deleting {thread.name} needs --yes when there's no terminal to ask on")
        lost = threads.losses(repo, thread)
        if lost:
            print(f"Deleting {thread.name} loses:")
            for item in lost:
                print(f"  {item}")
        else:
            print(f"Deleting {thread.name} loses nothing that isn't on your branches or origin's.")
        try:
            typed = input(f"Type {thread.name} to delete it: ").strip()
        except (EOFError, KeyboardInterrupt):
            typed = None
            print()
        if typed != thread.name:
            print("Not deleted.")
            return 1
    threads.delete(repo, thread)
    print(f"Deleted thread {thread.name}")
    return 0


def _configure(args: argparse.Namespace) -> int:
    if not args.feature and args.memory is None and not args.replace:
        args.parser.error("give --feature or --memory")
    repo = repos.current()
    spec = dict(repo.spec() or repos.DEFAULT_SPEC)
    if args.replace:
        spec = {"features": list(args.feature), "memory": args.memory or repos.DEFAULT_MEMORY}
    else:
        spec["features"] = repos.merge_features(list(spec.get("features", [])), args.feature)
        if args.memory is not None:
            spec["memory"] = args.memory
    spec = engine.validate_spec(spec)
    repo.save_spec(spec)
    print(f"Set {repo.name}'s spec: {_spec(spec)}")
    return 0


def _configure_show(args: argparse.Namespace) -> int:
    repo = repos.current()
    spec = repo.spec()
    print(_spec(spec) if spec else f"{_spec(repos.DEFAULT_SPEC)} (the default; nothing set yet)")
    return 0


def _repo_list(args: argparse.Namespace) -> int:
    rows = [("REPO", "PATH", "THREADS")]
    for repo in repos.known():
        where = str(repo.path) if repo.path.is_dir() else f"{repo.path} (gone)"
        rows.append((repo.key, where, str(len(threads.all_(repo)))))
    _table(rows)
    return 0


def _spec(spec: dict) -> str:
    return f"features [{', '.join(spec.get('features', []))}], memory {spec['memory']}"


def _size(size: int) -> str:
    for unit, scale in (("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)):
        if size >= scale:
            return f"{size / scale:.1f}{unit}"
    return f"{size}B"


def _ago(when: float) -> str:
    seconds = max(0, int(time.time() - when))
    for unit, size in (("d", 86400), ("h", 3600), ("m", 60)):
        if seconds >= size:
            return f"{seconds // size}{unit} ago"
    return "just now"


def _table(rows: list[tuple[str, ...]]) -> None:
    widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
    for row in rows:
        print("  ".join(cell.ljust(w) for cell, w in zip(row, widths)).rstrip())
