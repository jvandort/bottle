import argparse
import logging
import sys
import time
from pathlib import Path

from bottle import store
from bottle.errors import BottleError

log = logging.getLogger("bottle.cli")

def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="bottle", description="Sandboxed Linux environments for agents.")
    commands = parser.add_subparsers(required=True)

    auth = commands.add_parser("auth", help="log in once; bottles get the credentials their features need")
    auth_commands = auth.add_subparsers(required=True)
    auth_list = auth_commands.add_parser("list", help="list credentials features declare, and which are set")
    auth_list.set_defaults(run=_auth_list, parser=auth_list)
    auth_login = auth_commands.add_parser(
        "login",
        help="log in, with the credential's own login flow",
        description="Log in and store the credential in the Keychain. Runs the declaring feature's login "
        "command in a throwaway bottle, attached to this terminal, and captures the credential it prints.",
    )
    auth_login.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    auth_login.set_defaults(run=_auth_login, parser=auth_login)
    auth_set = auth_commands.add_parser(
        "set", help="store a credential read from stdin", description="Store a credential read from stdin."
    )
    auth_set.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    auth_set.set_defaults(run=_auth_set, parser=auth_set)
    auth_logout = auth_commands.add_parser("logout", help="remove a stored credential")
    auth_logout.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    auth_logout.set_defaults(run=_auth_logout, parser=auth_logout)

    build = commands.add_parser(
        "build",
        help="build a bottle image",
        description="Build the bottle image, with any features from containers/features installed on top.",
    )
    build.add_argument(
        "--feature", action="append", default=[], metavar="FEATURE",
        help="a feature to add, optionally with options, e.g. jvm:version=17 (repeatable)",
    )
    build.add_argument("--no-cache", action="store_true", help="rebuild every step")
    build.set_defaults(run=_build, parser=build)

    spec = commands.add_parser("spec", help="check a spec: a bottle's features and memory, as a file")
    spec_commands = spec.add_subparsers(required=True)
    spec_validate = spec_commands.add_parser(
        "validate",
        help="check a spec as `bottle new --spec` would, and print it in canonical form",
        description="Check a spec file as `bottle new --spec` would: its features exist, their options are "
        "valid and every required one is set, and the memory fits this machine. Prints it in canonical form.",
    )
    spec_validate.add_argument("file", metavar="FILE", help='the spec, e.g. {"features": ["tools"], "memory": "8G"}')
    spec_validate.add_argument("--json", action="store_true", help="print the canonical spec as JSON")
    spec_validate.set_defaults(run=_spec_validate, parser=spec_validate)

    new = commands.add_parser(
        "new",
        help="create a bottle from a spec",
        usage="bottle new --spec FILE --name NAME [--mount PATH=REPO [--checkout PATH=REF] "
        "[--git-remote PATH:REMOTE]...]... [--context FILE]",
        description="Create a bottle, in its own VM, with the spec's features and memory, and each --mount's repo "
        "checked out at its path. The first mount is where a shell or a command starts. NAME is required, "
        "and a name that's taken is refused.",
    )
    new.add_argument("--spec", metavar="FILE", required=True,
                     help='its features and memory, e.g. {"features": ["tools"], "memory": "8G"}')
    new.add_argument("--name", required=True, help="its name, which is its identity")
    new.add_argument(
        "--mount", action="append", default=[], metavar="PATH=REPO",
        help="check out REPO at PATH, its objects mounted read-only and its remotes served (repeatable)",
    )
    new.add_argument(
        "--git-remote", action="append", default=[], metavar="PATH:REMOTE",
        help="a remote to serve the mount at PATH, NAME:fetch=VIEW[,push=ns/NAMESPACE], VIEW being upstream, "
        "heads or ns/NAMESPACE (repeatable; a mount given none gets origin:fetch=upstream and host:fetch=heads)",
    )
    new.add_argument(
        "--checkout", action="append", default=[], metavar="PATH=REF",
        help="the branch or commit to check out at PATH (default: origin's default branch, "
        "else what the repo has checked out)",
    )
    new.add_argument("--context", metavar="FILE", help="add this file to what the bottle tells its agent (~/BOTTLE.md)")
    new.set_defaults(run=_new, parser=new)

    status = commands.add_parser(
        "status", help="show bottles' state",
        description="Show every bottle's state and memory; or, for those named, everything about them: "
        "features, mounts, remotes. A name with no bottle shows as missing.",
    )
    status.add_argument("names", nargs="*", metavar="BOTTLE", help="the bottles to show in full (default: all, in brief)")
    status.add_argument("--json", action="store_true", help="print JSON: one object per bottle, with what it was made from")
    status.set_defaults(run=_status, parser=status)

    shell = commands.add_parser("shell", help="open a shell in a bottle", description="Open a shell in a bottle, starting it if needed.")
    shell.add_argument("name", metavar="BOTTLE", help="the bottle to open a shell in")
    shell.set_defaults(run=_shell, parser=shell)

    exec_ = commands.add_parser(
        "exec",
        help="run a command in a bottle",
        description="Run one command in a bottle, at /workspace, and exit with its status. "
        "Starts the bottle if it's stopped. Put -- before the command if it takes options of its own.",
    )
    exec_.add_argument("name", metavar="BOTTLE", help="the bottle to run in")
    exec_.add_argument("command", nargs=argparse.REMAINDER, metavar="COMMAND", help="the command and its arguments")
    exec_.set_defaults(run=_exec, parser=exec_)

    start = commands.add_parser("start", help="start a bottle", description="Start a stopped bottle and its network access.")
    start.add_argument("name", metavar="BOTTLE", help="the bottle to start")
    start.set_defaults(run=_start, parser=start)

    stop = commands.add_parser(
        "stop", help="stop a bottle", description="Stop a bottle's VM. Its checkout and changes are kept."
    )
    stop.add_argument("name", metavar="BOTTLE", help="the bottle to stop")
    stop.set_defaults(run=_stop, parser=stop)

    delete = commands.add_parser(
        "delete",
        help="delete a bottle",
        description="Delete a bottle and everything it created. Also cleans up a bottle left half-made.",
    )
    delete.add_argument("name", metavar="BOTTLE", help="the bottle to delete (not its repo)")
    delete.set_defaults(run=_delete, parser=delete)

    shutdown = commands.add_parser(
        "shutdown", help="stop every bottle and bottled",
        description="Stop every running bottle, then bottled."
    )
    shutdown.set_defaults(run=_shutdown, parser=shutdown)

    daemon = commands.add_parser(
        "daemon",
        help="control bottled, which decides bottles' connections (started automatically when needed)",
    )
    daemon_commands = daemon.add_subparsers(required=True)
    daemon_start = daemon_commands.add_parser("start", help="start bottled in the background")
    daemon_start.add_argument("--foreground", action="store_true", help="run in this process instead")
    daemon_start.set_defaults(run=_daemon_start, parser=daemon_start)
    daemon_stop = daemon_commands.add_parser(
        "stop", help="stop bottled; running bottles lose network access until it's started again"
    )
    daemon_stop.set_defaults(run=_daemon_stop, parser=daemon_stop)

    args = parser.parse_args(argv)
    store.log_to_file("bottled" if args.run is _daemon_start and args.foreground else "bottle")
    command = " ".join(sys.argv[1:] if argv is None else argv)
    log.info("bottle %s", command)
    started = time.monotonic()
    try:
        status = args.run(args)
    except BottleError as e:
        log.warning("bottle %s: failed after %.1fs: %s", command, time.monotonic() - started, e)
        print(f"bottle: error: {e}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        log.warning("bottle %s: interrupted after %.1fs", command, time.monotonic() - started)
        raise
    log.info("bottle %s: exit %s after %.1fs", command, status, time.monotonic() - started)
    return status


def _log_to_stderr(level: int) -> None:
    """Also show bottle's log records at `level` and up on stderr, as `bottle: ...`."""
    handler = logging.StreamHandler()
    handler.setLevel(level)
    handler.setFormatter(logging.Formatter("bottle: %(message)s"))
    # How the command went is for the file; the terminal already has its output.
    handler.addFilter(lambda record: record.name != log.name)
    logging.getLogger("bottle").addHandler(handler)


def _features(specs) -> str:
    return f"[{', '.join(sorted(specs))}]"


def _settings(bottle) -> str:
    """Its features, and its memory unless it has no limit: [claude, tools] and memory 8G."""
    return _features(bottle.features) + (f" and memory {bottle.memory}" if bottle.memory else "")


def _table(rows: list[tuple[str, ...]]) -> None:
    widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
    for row in rows:
        print("  ".join(cell.ljust(w) for cell, w in zip(row, widths)).rstrip())


def _auth_list(args: argparse.Namespace) -> int:
    from bottle import auth

    # Every credential is held by bottled, so what's worth showing is
    # where it's spent: the hosts it's attached to, as the feature declares
    # them (${option} is filled in from a bottle's own options).
    rows = [("CREDENTIAL", "FEATURE", "STATUS", "ATTACHED TO", "DESCRIPTION")]
    rows += [(d.credential.name, d.feature, _credential_status(d, is_set),
              ", ".join(d.credential.inject.hosts), d.credential.description)
             for d, is_set in auth.status()]
    _table(rows)
    return 0


def _credential_status(declared, is_set: bool) -> str:
    """Whether the Keychain has it, and the file read before the Keychain, if the credential has one."""
    from bottle import features

    status = "set" if is_set else "not set"
    if declared.credential.properties:
        values = features.load(declared.feature).values
        path = features.OPTION_REFERENCE.sub(lambda m: str(values[m.group(1)]), declared.credential.properties)
        status = f"{path}, else {status}"
    return status


def _auth_login(args: argparse.Namespace) -> int:
    from bottle import auth

    auth.login(args.name)
    print(f"Logged in: {args.name}. Bottles with it get it when they next start.")
    return 0


def _auth_set(args: argparse.Namespace) -> int:
    import getpass

    from bottle import auth

    value = getpass.getpass(f"{args.name}: ") if sys.stdin.isatty() else sys.stdin.read()
    auth.set_value(args.name, value)
    print(f"Stored {args.name}. Bottles with it get it when they next start.")
    return 0


def _auth_logout(args: argparse.Namespace) -> int:
    from bottle import auth

    print(f"Removed {args.name}" if auth.logout(args.name) else f"{args.name} wasn't set")
    return 0


def _spec_validate(args: argparse.Namespace) -> int:
    import json

    from bottle import specs

    spec = specs.read(Path(args.file).expanduser())
    if args.json:
        print(json.dumps(spec))
    else:
        print(f"Valid: features {_features(spec['features'])} and memory {spec['memory']}")
    return 0


def _new(args: argparse.Namespace) -> int:
    from bottle import bottles, specs

    context = _read_context(args.context)
    spec = specs.read(Path(args.spec).expanduser())
    print("Creating a bottle...", file=sys.stderr)
    bottle = bottles.create(spec, args.name, _mount_requests(args), context)
    print(f"Created {_describe(bottle)}")
    print(f"Open a shell with: bottle shell {bottle.name}")
    return 0


def _mount_requests(args: argparse.Namespace):
    """--mount, --checkout and --git-remote, as the mounts they describe."""
    from bottle import bottles

    mounts: dict[str, Path] = {}
    for given in args.mount:
        path, equals, repo = given.partition("=")
        if not equals or not path or not repo:
            raise BottleError(f"invalid --mount {given!r}: use PATH=REPO, e.g. /workspace=~/src/app")
        if path in mounts:
            raise BottleError(f"two mounts at {path}")
        mounts[path] = Path(repo).expanduser()
    checkouts: dict[str, str] = {}
    for given in args.checkout:
        path, equals, ref = given.partition("=")
        if not equals or path not in mounts:
            raise BottleError(f"invalid --checkout {given!r}: use PATH=REF, PATH a --mount's")
        checkouts[path] = ref
    remotes: dict[str, list[str]] = {}
    for given in args.git_remote:
        path, colon, remote = given.partition(":")
        if not colon or path not in mounts:
            raise BottleError(f"invalid --git-remote {given!r}: use PATH:NAME:fetch=VIEW..., PATH a --mount's")
        remotes.setdefault(path, []).append(remote)
    return [bottles.MountRequest(path, repo, checkouts.get(path), remotes.get(path)) for path, repo in mounts.items()]


def _read_context(path: str | None) -> str:
    if path is None:
        return ""
    try:
        return Path(path).expanduser().read_text()
    except OSError as e:
        raise BottleError(f"can't read context {path}: {e.strerror or e}") from None


def _describe(bottle) -> str:
    mounted = ", ".join(f"{m.repo} at {m.checkout} on {m.path}" for m in bottle.mounts) or "no repo"
    return f"bottle '{bottle.name}', with {mounted} and features {_settings(bottle)}"


def _shell(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.shell(args.name)
    return 0  # not reached: shell replaces this process


def _status(args: argparse.Namespace) -> int:
    import json

    from bottle import bottles

    rows = bottles.status(args.names)
    if args.json:
        print(json.dumps(rows, indent=2))
        return 0
    if not args.names:
        table = [("NAME", "STATE", "MEMORY")]
        table += [(row["name"], row["state"], row.get("memory") or "-") for row in rows]
        _table(table)
        return 0
    for n, row in enumerate(rows):
        print(("\n" if n else "") + f"{row['name']}  {row['state']}")
        if row["state"] == "missing":
            continue
        print(f"  memory    {row['memory']}")
        print(f"  features  {', '.join(row['features']) or '-'}")
        for mount in row["mounts"]:
            at = mount["branch"] or f"({mount['commit'][:12]})"
            print(f"  {mount['path']}  {mount['repo']} at {at}")
            for remote in mount["remotes"]:
                print(f"    {remote}")
        if not row["mounts"]:
            print("  no repo")
    return 0


def _delete(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.delete(args.name)
    print(f"Deleted {args.name}")
    return 0


def _build(args: argparse.Namespace) -> int:
    from bottle import features, images

    print(f"Built {features.build(images.BASE, args.feature, args.no_cache)}")
    return 0


def _exec(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.exec_(args.name, args.command)  # replaces this process


def _start(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.start(args.name)
    print(f"Started {args.name}")
    return 0


def _stop(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.stop(args.name)
    print(f"Stopped {args.name}")
    return 0


def _shutdown(args: argparse.Namespace) -> int:
    from bottle import bottles

    stopped, daemon_was_running = bottles.shutdown()
    for name in stopped:
        print(f"Stopped {name}")
    if daemon_was_running:
        print("Stopped bottled")
    if not stopped and not daemon_was_running:
        print("Nothing was running")
    return 0


def _daemon_stop(args: argparse.Namespace) -> int:
    from bottle import daemon

    print("Stopped bottled" if daemon.stop() else "bottled isn't running")
    return 0


def _daemon_start(args: argparse.Namespace) -> int:
    import asyncio

    from bottle import daemon

    if not args.foreground:
        print("Started bottled" if daemon.start() else "bottled is already running")
        return 0
    # It logs to bottled.log; a terminal running it by hand sees that too.
    if sys.stderr.isatty():
        _log_to_stderr(logging.INFO)
    # A crash is logged, stamped, rather than left to a bare traceback on stderr.
    sys.excepthook = lambda *exc_info: log.critical("bottled crashed", exc_info=exc_info)
    try:
        asyncio.run(daemon.serve())
    except KeyboardInterrupt:
        pass
    return 0
