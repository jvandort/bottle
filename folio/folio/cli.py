"""What you can type, and what happens when you do."""

import argparse
import sys
import time

from . import broker, engine, notes, obsidian, picker, repos, threads
from .errors import FolioError


PICK = "choose one from a list"


DISK_HELP = "the disk it writes to, in G, e.g. 50G (default: 20G)"
RUNNER_HELP = "the runner to make its bottle on (default: the repo's, from `folio configure`, else this machine)"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="folio", description="Threads of work in bottles: each one a bottle, a lane for its commits, and Claude.",
    )
    commands = parser.add_subparsers(required=True)

    new = commands.add_parser(
        "new", help="start a thread",
        description="Start a thread in the repo you're in: a bottle for it, and a lane for its commits, "
        "fetched from the remote bottle/THREAD. Then `folio claude THREAD` or `folio shell THREAD` to work in it.",
    )
    new.add_argument("thread", metavar="THREAD", help="its name: lowercase words joined by '-', e.g. fix-flaky-test")
    new.add_argument("--from", dest="base", metavar="BRANCH",
                     help="the branch to start from (default: origin's default branch)")
    new.add_argument("--runner", metavar="RUNNER", help=RUNNER_HELP)
    new.set_defaults(run=_new)

    for name, session, what in (("claude", threads.CLAUDE, "Claude"), ("shell", threads.SHELL, "shell")):
        attach = commands.add_parser(
            name, help=f"attach to a thread's {what}",
            description=f"Attach to the thread's {what}, starting its bottle and the session if they've stopped, "
            "or starting the thread itself, as `folio new` does, if there's none of that name. "
            "Closing the terminal only detaches: what's running in it carries on.",
        )
        attach.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
        attach.add_argument("--runner", metavar="RUNNER", help=RUNNER_HELP + "; a paused thread resumes there")
        attach.set_defaults(run=_attach, command=name, session=session)

    pause = commands.add_parser(
        "pause", help="snapshot a thread and delete its bottle",
        description="Snapshot the thread -- its workspace, every branch and stash, uncommitted changes, and "
        "Claude's conversation -- and delete its bottle. Only running processes are lost. `folio claude` or "
        "`folio shell` resumes it, in a new bottle made from the repo's spec as it is then.",
    )
    pause.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
    pause.set_defaults(run=_pause, command="pause")

    notes_ = commands.add_parser(
        "notes", help="open a thread's notes in Obsidian",
        description="Open the thread's notes -- the repo its agent keeps at /notes -- in Obsidian, adding them "
        "as a vault the first time. A deleted thread's notes are kept, and open the same way.",
    )
    notes_.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
    notes_.set_defaults(run=_notes, command="notes")

    rename = commands.add_parser(
        "rename", help="rename a paused thread",
        description="Rename a thread: its record, its lane, and the bottle/THREAD remote, which your branches "
        "tracking it follow. Its branches keep their names. A thread with a bottle is paused first: "
        "`folio pause THREAD`.",
    )
    rename.add_argument("thread", metavar="THREAD", help="the thread")
    rename.add_argument("new", metavar="NEW", help="its new name: lowercase words joined by '-'")
    rename.set_defaults(run=_rename)

    list_ = commands.add_parser("list", aliases=["ls", "status"], help="the repo's threads", description="The repo's threads, most recently active first.")
    list_.set_defaults(run=_list)

    delete = commands.add_parser(
        "delete", help="delete a thread",
        description="Delete a thread: its bottle, its lane and the bottle/THREAD remote, and what folio knows "
        "about it. Says first what that would lose, and asks you to type the thread's name.",
    )
    delete.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
    delete.add_argument("--yes", action="store_true", help="don't ask (needed without a terminal)")
    delete.set_defaults(run=_delete, command="delete")

    configure = commands.add_parser(
        "configure", help="the repo's spec: its threads' features, memory and disk, and its runner",
        description="The repo's spec: shown without options, changed with them. Bottle checks it before it's saved. "
        "Threads get it in their next bottle; a bottle already running keeps what it has.",
    )
    _configure_arguments(configure, runner_help="the runner the repo's threads' bottles are made on, unless a "
                         "thread is given its own (`folio runner ls` lists them; default: this machine)")
    configure.set_defaults(run=_configure, parser=configure, command="configure")

    sidecar = commands.add_parser(
        "sidecar", help="a thread's sidecar: a bottle of its own on another runner, which the thread ssh-es to",
        description="A thread's sidecar: a bottle with a spec and runner of its own (a bigger machine, say), the "
        "thread's repos fetch-only, which the thread's bottle reaches as `ssh sidecar`. It's scratch: made and "
        "started with the thread's bottle, and deleted with it, a pause included. A change reaches a thread "
        "with a bottle when it's paused and resumed.",
    )
    sidecar_commands = sidecar.add_subparsers(required=True)
    command = sidecar_commands.add_parser(
        "configure", help="the thread's sidecar, as `folio configure` the repo's spec: --runner gives it one",
        description="The thread's sidecar: shown without options, changed with them, as `folio configure` does the "
        "repo's spec; a thread without one is given one, which needs --runner. Bottle checks its spec before it's "
        "saved.",
    )
    command.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
    _configure_arguments(command, runner_help="the runner it's made on (`folio runner ls` lists them)")
    command.set_defaults(run=_sidecar_configure, parser=command, command="sidecar configure")
    for name, help_, run in (("remove", "take the thread's sidecar away", _sidecar_remove),
                             ("shell", "a shell in the running thread's sidecar, at /workspace, for as long as you're "
                              "in it", _sidecar_shell)):
        command = sidecar_commands.add_parser(name, help=help_)
        command.add_argument("thread", metavar="THREAD", nargs="?", help=f"the thread (default: {PICK})")
        command.set_defaults(run=run, command=f"sidecar {name}")

    broker_ = commands.add_parser(
        "broker", help="the broker: the thread that talks to the others, and keeps the notes",
        description="The broker: one thread outside your repos, whose agent talks to the others' and keeps "
        "a long-term memory of it all, as notes in its vault, an Obsidian vault folio owns. "
        "`claude` (the default) and `shell` attach to it, starting it if need be; `pause` pauses it, as "
        "`folio pause` does a thread; `notes` opens its vault in Obsidian.",
    )
    broker_.add_argument("command", nargs="?", default="claude", choices=("claude", "shell", "pause", "notes"),
                         help="what to do (default: claude)")
    broker_.set_defaults(run=_broker)

    repo = commands.add_parser("repo", help="the repos folio knows")
    repo_commands = repo.add_subparsers(required=True)
    repo_list = repo_commands.add_parser("list", aliases=["ls"], help="every repo folio has state for, wherever you are")
    repo_list.set_defaults(run=_repo_list)

    serve = commands.add_parser(
        "serve", help="what bottles' machines route to folio: their git, credentials, ssh and messages",
        description="Start or stop folio serve, which answers what bottles' machines route to it, on sockets in "
        "$FOLIO_HOME/services: bottle.host (their repos' git, messages between bottles), ssh to bottles, and "
        "credentials' hosts. bottle starts it when it's needed; its log is $FOLIO_HOME/logs/serve.log.",
    )
    serve_commands = serve.add_subparsers(required=True)
    serve_start = serve_commands.add_parser("start", help="start it in the background")
    serve_start.add_argument("--foreground", action="store_true", help="run it in this process instead")
    serve_start.set_defaults(run=_serve_start)
    serve_stop = serve_commands.add_parser("stop", help="stop it; bottles' machines go on, but for what they route")
    serve_stop.set_defaults(run=_serve_stop)

    auth_ = commands.add_parser("auth", help="credentials: set once; bottles get those their features need")
    auth_commands = auth_.add_subparsers(required=True)
    command = auth_commands.add_parser("list", help="list credentials features declare, and which are set")
    command.set_defaults(run=_auth_list)
    command = auth_commands.add_parser("set", help="store a credential in the Keychain, read from stdin",
                                       description="Store a credential in the Keychain, read from stdin (asked for, "
                                       "unechoed, on a terminal): `claude setup-token` makes claude's.")
    command.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    command.set_defaults(run=_auth_set)
    command = auth_commands.add_parser("remove", aliases=["rm"], help="remove a stored credential")
    command.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    command.set_defaults(run=_auth_remove)

    runner = commands.add_parser(
        "runner", help="the other machines folio makes bottles on: add|ls|rm|ssh|check|detach|attach",
        description="The other machines folio makes bottles on, over ssh: folio-cli's runner commands (`folio runner "
        "add NAME --ssh DESTINATION`, `ls`, `rm NAME`, `ssh NAME [COMMAND]`, `check NAME|DESTINATION`, `detach NAME "
        "[--offline]`, `attach NAME`). A runner folio has bottles on is removed only once they're deleted.",
    )
    runner.add_argument("arguments", nargs=argparse.REMAINDER, metavar="...", help="folio-cli runner's")
    runner.set_defaults(run=_runner_command)

    args = parser.parse_args(argv)
    try:
        return args.run(args)
    except (FolioError, engine.BottleError) as e:
        print(f"folio: error: {e}", file=sys.stderr)
        return 1


def _runner_command(args: argparse.Namespace) -> int:
    """folio-cli's runner commands, folio-cli running in this process's place: but for removing a runner folio still
    has bottles on, refused here, as folio-cli doesn't know them (yet)."""
    import os

    from .bottle import bottles, krun

    if args.arguments[:1] == ["rm"] and len(args.arguments) > 1:
        name = args.arguments[1]
        there = sorted(b.name for b in bottles.load().values() if b.runner == name)
        if there:
            raise FolioError(f"runner {name} has bottles: {', '.join(there)}; delete their threads first")
    program = krun.folio_binary()
    try:
        os.execvp(program, [program, "runner", *args.arguments])
    except FileNotFoundError:
        raise FolioError(krun.RunnerCli("").missing()) from None


def _new(args: argparse.Namespace) -> int:
    thread = _start(repos.current(), args.thread, args.base, args.runner)
    print(f"Work in it with: folio claude {thread.name}  (or folio shell {thread.name})")
    return 0


def _start(repo: repos.Repo, name: str, base: str | None = None, runner: str | None = None) -> threads.Thread:
    print(f"Starting thread {name} in {repo.path}...", file=sys.stderr)
    thread = threads.create(repo, name, base, runner=runner)
    # Flushed: attaching next replaces this process, and with it anything still buffered.
    print(f"Started thread {thread.name} in bottle {thread.bottle}. Its branches: git fetch bottle/{thread.name}",
          flush=True)
    return thread


def _thread(args: argparse.Namespace, repo: repos.Repo, new: bool = False) -> threads.Thread | None:
    """The thread the command names (started, if it's new and the command may start one), else the one
    chosen in the picker; None if none was."""
    if args.thread is not None:
        if new and not threads.exists(repo, args.thread):
            return _start(repo, args.thread, runner=getattr(args, "runner", None))
        return threads.load(repo, args.thread)
    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        raise FolioError(f"give the thread: `folio {args.command} THREAD` (choosing one needs a terminal)")
    all_ = threads.all_(repo)
    if not all_ and not new:
        raise FolioError(f"{repo.path} has no threads; start one with `folio new THREAD`")
    states = threads.states(repo, all_)
    all_ = threads.recent(repo, all_, states)
    width = max((len(t.name) for t in all_), default=0)
    choices = [picker.NEW] if new else []
    choices += [picker.Choice(f"{t.name:<{width}}  {states[t.name]['state']:<8}  {_ago(threads.last_active(repo, t, states[t.name]))}", t.name)
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
    if args.runner is not None:
        thread = threads.place(repo, thread, args.runner)
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


def _broker(args: argparse.Namespace) -> int:
    identity = broker.repo()
    thread = threads.load(identity, broker.NAME) if threads.exists(identity, broker.NAME) else None
    state = threads.states(identity, [thread])[broker.NAME]["state"] if thread else "new"
    if state in ("new", "missing", "paused") and broker.migrate(identity):
        print(f"Moved the broker's notes to {broker.notes_path(identity)}", file=sys.stderr)
    elif broker.old_vault() is not None and args.command != "pause":
        print(f"The broker's notes move to {broker.notes_path(identity)} when it's next paused "
              "(`folio broker pause`) and resumed", file=sys.stderr)
    if args.command == "notes":
        old = broker.old_vault()
        return _open_notes(old if old is not None else notes.ensure(broker.notes_path(identity), "The broker's notes"))
    if thread is None and args.command == "pause":
        raise FolioError("there's no broker to pause; `folio broker` starts it")
    if thread is None:
        print("Starting the broker...", file=sys.stderr)
        thread = threads.create(identity, broker.NAME, broker_=True)
        print(f"Started the broker. Its notes: {broker.notes_path(identity)}", flush=True)
    if args.command == "pause":
        print("Pausing the broker...", file=sys.stderr)
        threads.pause(identity, thread)
        if broker.migrate(identity):
            print(f"Moved the broker's notes to {broker.notes_path(identity)}", file=sys.stderr)
        print("Paused the broker. Resume it with: folio broker")
        return 0
    threads.attach(identity, thread, threads.CLAUDE if args.command == "claude" else threads.SHELL)


def _open_notes(path) -> int:
    """Open the vault in Obsidian, if it's installed; else say where it is."""
    if obsidian.installed():
        print(obsidian.open_vault(path))
    else:
        print(f"The notes are in {path}: open that folder as a vault in Obsidian.")
    return 0


def _notes(args: argparse.Namespace) -> int:
    repo = repos.current()
    if args.thread is not None:
        name = args.thread                          # a deleted thread's notes too
    else:
        thread = _thread(args, repo)
        if thread is None:
            return 0
        name = thread.name
    path = threads.notes_dir(repo, name)
    if not path.exists():
        raise FolioError(f"thread {name} has no notes yet: its bottle starts them")
    return _open_notes(path)


def _rename(args: argparse.Namespace) -> int:
    repo = repos.current()
    thread = threads.rename(repo, threads.load(repo, args.thread), args.new)
    print(f"Renamed thread {args.thread} to {thread.name}. Its branches: git fetch bottle/{thread.name}; "
          f"resume it with: folio claude {thread.name}")
    return 0


def _list(args: argparse.Namespace) -> int:
    repo = repos.current()
    all_ = threads.all_(repo)
    states = threads.states(repo, all_)
    all_ = threads.recent(repo, all_, states)
    rows = [("THREAD", "STATE", "RUNNER", "SIDECAR", "SNAPSHOT", "ACTIVE", "FEATURES")]
    for thread in all_:
        row = states[thread.name]
        size = threads.snapshot_size(repo, thread) if row["state"] == "paused" else None
        # Where its bottle is; for one without, where its next is made.
        runner = row.get("runner") or threads.runner_of(repo, thread) or "local"
        # Its sidecar's runner, and how it is, unless there's none to speak of (a paused thread's).
        sidecar = "-"
        if thread.sidecar:
            sidecar = thread.sidecar["runner"] + ("" if row["sidecar"] == "missing" else f" ({row['sidecar']})")
        rows.append((thread.name, row["state"], runner, sidecar, _size(size) if size is not None else "-",
                     _ago(threads.last_active(repo, thread, row)), " ".join(row.get("features", ())) or "-"))
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


def _configure_arguments(command: argparse.ArgumentParser, runner_help: str) -> None:
    """What `folio configure` and `folio sidecar configure` change, the same for both."""
    command.add_argument("--feature", action="append", default=[], metavar="FEATURE",
                         help="add a feature, optionally with options, e.g. jvm:version=21; one there already gets "
                         "these options (repeatable)")
    command.add_argument("--no-feature", action="append", default=[], metavar="FEATURE",
                         help="take a feature away, by its name alone, e.g. jvm (repeatable)")
    command.add_argument("--memory", help=f"memory for each bottle, in M or G, e.g. 8G (default: {repos.DEFAULT_MEMORY})")
    disk = command.add_mutually_exclusive_group()
    disk.add_argument("--disk", help=DISK_HELP)
    disk.add_argument("--no-disk", action="store_true", help="the default disk again")
    runner = command.add_mutually_exclusive_group()
    runner.add_argument("--runner", metavar="RUNNER", help=runner_help)
    runner.add_argument("--no-runner", action="store_true", help="this machine again")


def _changes_spec(args: argparse.Namespace) -> bool:
    return bool(args.feature or args.no_feature or args.memory is not None or args.disk is not None or args.no_disk)


def _changes(args: argparse.Namespace) -> bool:
    """Whether a configure command was given anything to change: else it shows."""
    return _changes_spec(args) or args.runner is not None or args.no_runner


def _changed(spec: dict, args: argparse.Namespace) -> dict:
    """`spec` as a configure command changes it: features added (one there already gets the options given) or taken
    away, memory or disk set, or the disk the default again."""
    spec = dict(spec)
    for name in args.no_feature:
        if ":" in name or "=" in name:
            args.parser.error(f"--no-feature takes a feature's name alone, e.g. {name.split(':', 1)[0]}")
    both = sorted({given.split(":", 1)[0] for given in args.feature} & set(args.no_feature))
    if both:
        args.parser.error(f"--feature and --no-feature both name {', '.join(both)}")
    features = repos.merge_features(list(spec.get("features", [])), args.feature)
    there = [f.split(":", 1)[0] for f in features]
    for name in args.no_feature:
        if name not in there:
            raise FolioError(f"there's no feature {name} to take away (features: {', '.join(there) or 'none'})")
    spec["features"] = [f for f in features if f.split(":", 1)[0] not in args.no_feature]
    if args.memory is not None:
        spec["memory"] = args.memory
    if args.disk is not None:
        spec["disk"] = args.disk
    if args.no_disk:
        spec.pop("disk", None)
    return spec


def _configure(args: argparse.Namespace) -> int:
    repo = repos.current()
    if not _changes(args):
        spec = repo.spec()
        print((_spec(spec) if spec else f"{_spec(repos.DEFAULT_SPEC)} (the default; nothing set yet)") + _runner(repo))
        return 0
    if _changes_spec(args):
        # Checked before anything's saved, the runner included.
        spec = engine.validate_spec(repos.check_repo_spec(_changed(dict(repo.spec() or repos.DEFAULT_SPEC), args)))
    if args.runner is not None or args.no_runner:
        repo.save_runner(args.runner)
    if _changes_spec(args):
        repo.save_spec(spec)
        print(f"Set {repo.name}'s spec: {_spec(spec)}{_runner(repo)}")
    else:
        print(f"Set {repo.name}'s runner: {repo.runner() or 'this machine'}")
    return 0


def _sidecar_configure(args: argparse.Namespace) -> int:
    repo = repos.current()
    thread = _thread(args, repo)
    if thread is None:
        return 0
    if not _changes(args):
        if thread.sidecar is None:
            print(f"Thread {thread.name} has no sidecar")
            return 0
        state = engine.status([thread.sidecar_bottle])[thread.sidecar_bottle]["state"]
        print(f"{_sidecar_text(thread.sidecar)}; bottle {thread.sidecar_bottle}, {state}")
        return 0
    if args.no_runner:
        raise FolioError(f"a sidecar is on a runner of its own; `folio sidecar remove {thread.name}` takes it away")
    if thread.sidecar is None and args.runner is None:
        raise FolioError(f"thread {thread.name} has no sidecar; give it one with "
                         f"`folio sidecar configure {thread.name} --runner RUNNER ...`")
    current = thread.sidecar or {"runner": args.runner, "spec": {"features": [], "memory": repos.DEFAULT_MEMORY}}
    sidecar = {"runner": args.runner or current["runner"], "spec": _changed(current["spec"], args)}
    thread = threads.set_sidecar(repo, thread, sidecar)
    print(f"Set thread {thread.name}'s sidecar: {_sidecar_text(thread.sidecar)}{_when(repo, thread)}")
    return 0


def _sidecar_remove(args: argparse.Namespace) -> int:
    repo = repos.current()
    thread = _thread(args, repo)
    if thread is None:
        return 0
    if thread.sidecar is None:
        print(f"Thread {thread.name} has no sidecar")
        return 0
    thread = threads.set_sidecar(repo, thread, None)
    print(f"Took thread {thread.name}'s sidecar away{_when(repo, thread)}")
    return 0


def _sidecar_shell(args: argparse.Namespace) -> int:
    repo = repos.current()
    thread = _thread(args, repo)
    if thread is None:
        return 0
    threads.sidecar_shell(repo, thread)


def _sidecar_text(sidecar: dict) -> str:
    return f"{_spec(sidecar['spec'])}, runner {sidecar['runner']}"


def _when(repo: repos.Repo, thread: threads.Thread) -> str:
    """When a change to its sidecar reaches the thread: at once, for one with no bottle."""
    if threads.states(repo, [thread])[thread.name]["state"] in ("paused", "missing"):
        return ""
    return f". It comes with the thread's next bottle: `folio pause {thread.name}`, then `folio claude {thread.name}`"


def _runner(repo: repos.Repo) -> str:
    return f", runner {repo.runner()}" if repo.runner() else ""


def _daemon():
    """bottle's daemon module, which folio serve is for now: shared, until it's folio's own."""
    from .bottle import daemon   # engine has put bottle's package where it's found

    return daemon


def _serve_start(args: argparse.Namespace) -> int:
    daemon = _daemon()
    if not args.foreground:
        print("Started folio serve" if daemon.start() else "folio serve is already running")
        return 0
    import asyncio
    import logging

    from .bottle import store

    store.log_to_file(daemon.LOG, daemon.home())
    # A terminal running it by hand sees what it logs, too.
    if sys.stderr.isatty():
        handler = logging.StreamHandler()
        handler.setFormatter(logging.Formatter(store.LOG_FORMAT))
        logging.getLogger().addHandler(handler)
    # A crash is logged, stamped, rather than left to a bare traceback on stderr.
    sys.excepthook = lambda *exc_info: logging.getLogger("folio.serve").critical("folio serve crashed",
                                                                                exc_info=exc_info)
    try:
        asyncio.run(daemon.serve())
    except KeyboardInterrupt:
        pass
    return 0


def _serve_stop(args: argparse.Namespace) -> int:
    print("Stopped folio serve" if _daemon().stop() else "folio serve isn't running")
    return 0


def _auth_list(args: argparse.Namespace) -> int:
    from .bottle import auth

    # Every credential is held by folio serve, so what's worth showing is where it's spent: the hosts it's attached
    # to, as the feature declares them (${option} is filled in from a bottle's own options).
    rows = [("CREDENTIAL", "FEATURE", "STATUS", "ATTACHED TO", "DESCRIPTION")]
    rows += [(d.credential.name, d.feature, _credential_status(d, is_set),
              ", ".join(d.credential.inject.hosts), d.credential.description)
             for d, is_set in auth.status()]
    _table(rows)
    return 0


def _credential_status(declared, is_set: bool) -> str:
    """Whether the Keychain has it, and the file read before the Keychain, if the credential has one."""
    from .bottle import features

    status = "set" if is_set else "not set"
    if declared.credential.properties:
        values = features.load(declared.feature).values
        path = features.OPTION_REFERENCE.sub(lambda m: str(values[m.group(1)]), declared.credential.properties)
        status = f"{path}, else {status}"
    return status


def _auth_set(args: argparse.Namespace) -> int:
    import getpass

    from .bottle import auth

    value = getpass.getpass(f"{args.name}: ") if sys.stdin.isatty() else sys.stdin.read()
    auth.set_value(args.name, value)
    print(f"Stored {args.name}. Bottles with it get it when they next start.")
    return 0


def _auth_remove(args: argparse.Namespace) -> int:
    from .bottle import auth

    print(f"Removed {args.name}" if auth.remove(args.name) else f"{args.name} wasn't set")
    return 0


def _repo_list(args: argparse.Namespace) -> int:
    rows = [("REPO", "PATH", "THREADS")]
    for repo in repos.known():
        where = ("(the broker)" if repo.path == repos.owned_path(broker.NAME)
                 else str(repo.path) if repo.path.is_dir() else f"{repo.path} (gone)")
        rows.append((repo.key, where, str(len(threads.all_(repo)))))
    _table(rows)
    return 0


def _spec(spec: dict) -> str:
    return f"features [{', '.join(spec.get('features', []))}], memory {spec['memory']}" + (
        f", disk {spec['disk']}" if "disk" in spec else "")


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
