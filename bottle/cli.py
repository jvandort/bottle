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
    build.add_argument("--runner", default="local", metavar="RUNNER",
                       help="build it there, for the bottles it runs (default: local, this machine)")
    build.set_defaults(run=_build, parser=build)

    spec = commands.add_parser("spec", help="check a spec: a bottle's features and memory, as a file")
    spec_commands = spec.add_subparsers(required=True)
    spec_validate = spec_commands.add_parser(
        "validate",
        help="check a spec as `bottle new --spec` would, and print it in canonical form",
        description="Check a spec file as `bottle new --spec` would: its features exist, their options are "
        "valid and every required one is set, and the memory is a size. Prints it in canonical form. Whether the "
        "memory fits a machine is `bottle new`'s to check, against the runner the bottle is for.",
    )
    spec_validate.add_argument("file", metavar="FILE", help='the spec, e.g. {"features": ["tools"], "memory": "8G"}')
    spec_validate.add_argument("--json", action="store_true", help="print the canonical spec as JSON")
    spec_validate.set_defaults(run=_spec_validate, parser=spec_validate)

    new = commands.add_parser(
        "new",
        help="create a bottle from a spec",
        usage="bottle new --spec FILE --name NAME [--mount PATH=REPO [--checkout PATH=REF] "
        "[--git-remote PATH:REMOTE]...]... [--context FILE] [--runner RUNNER] [--ssh [NAME=]BOTTLE]...",
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
    new.add_argument("--runner", default="local", metavar="RUNNER",
                     help="the machine its VM runs on (default: local, this one)")
    new.add_argument("--ssh", action="append", default=[], metavar="[NAME=]BOTTLE",
                     help="let it ssh to BOTTLE, which must exist, as host NAME (default: BOTTLE's own name), "
                     "e.g. sidecar.host=build (repeatable)")
    new.set_defaults(run=_new, parser=new)

    status = commands.add_parser(
        "status", aliases=["list", "ls"], help="show bottles' state",
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

    cp = commands.add_parser(
        "cp", help="copy a file or a directory into, out of, or between bottles",
        description="Copy SRC, a file or a directory, to DST: into DST if it's a directory, else as DST. "
        "BOTTLE:PATH is a path in that bottle (a relative one from /workspace), started if it's stopped; "
        "any other is a path here, and one side at least is in a bottle. Between bottles, it goes by way "
        "of this machine.",
    )
    cp.add_argument("src", metavar="SRC", help="what to copy: BOTTLE:PATH, or a path here")
    cp.add_argument("dst", metavar="DST", help="where to: BOTTLE:PATH, or a path here")
    cp.set_defaults(run=_cp, parser=cp)

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

    runner = commands.add_parser("runner", help="the machines bottles run on: this one, and others over ssh")
    runner_commands = runner.add_subparsers(required=True)
    runner_add = runner_commands.add_parser(
        "add", help="add a machine to run bottles on, over ssh",
        description="Add a Linux machine to run bottles on, reached over ssh as you reach it. Checks what it has, "
        "installs smolvm there if it's missing (asking first), and copies bottle's code over.",
    )
    runner_add.add_argument("name", metavar="RUNNER", help="its name, as `bottle new --runner` takes it")
    runner_add.add_argument("--ssh", required=True, metavar="DESTINATION",
                            help="where ssh reaches it: a Host from ~/.ssh/config, or user@host")
    runner_add.set_defaults(run=_runner_add, parser=runner_add)
    runner_status = runner_commands.add_parser(
        "status", aliases=["list", "ls"], help="show runners: reachable or not, what they're missing, their bottles")
    runner_status.add_argument("names", nargs="*", metavar="RUNNER", help="the runners to show (default: all)")
    runner_status.add_argument("--json", action="store_true", help="print JSON: one object per runner")
    runner_status.set_defaults(run=_runner_status, parser=runner_status)
    runner_stop = runner_commands.add_parser(
        "stop", help="stop every bottle on a runner, to shut the machine down",
        description="Stop every bottle on a runner, so the machine can be shut down. Their disks stay on it, "
        "and each starts again with `bottle start`.")
    runner_stop.add_argument("name", metavar="RUNNER", help="the runner whose bottles to stop")
    runner_stop.set_defaults(run=_runner_stop, parser=runner_stop)
    runner_detach = runner_commands.add_parser(
        "detach", help="stop everything on a runner and let it go, to turn it off",
        description="Stop every bottle on a runner, check nothing else runs there, and let it go: then it's safe to "
        "turn off. Until it's attached again it isn't asked anything, and its bottles show as detached. Using one "
        "of them attaches it again, if it's back; so does `bottle runner attach`.")
    runner_detach.add_argument("name", metavar="RUNNER", help="the runner to detach")
    runner_detach.add_argument("--offline", action="store_true",
                               help="it's off already: mark it detached without asking it anything")
    runner_detach.set_defaults(run=_runner_detach, parser=runner_detach)
    runner_attach = runner_commands.add_parser(
        "attach", help="attach a detached runner again, once it's back",
        description="Attach a detached runner again, once it answers, and name the bottles detaching it stopped.")
    runner_attach.add_argument("name", metavar="RUNNER", help="the runner to attach")
    runner_attach.set_defaults(run=_runner_attach, parser=runner_attach)
    runner_remove = runner_commands.add_parser(
        "remove", help="forget a runner with no bottles", description="Forget a runner with no bottles on it. "
        "Nothing on the machine is touched.")
    runner_remove.add_argument("name", metavar="RUNNER", help="the runner to forget")
    runner_remove.set_defaults(run=_runner_remove, parser=runner_remove)

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
    bottle = bottles.create(spec, args.name, _mount_requests(args), context, runner=args.runner, ssh=_ssh(args))
    print(f"Created {_describe(bottle)}")
    print(f"Open a shell with: bottle shell {bottle.name}")
    return 0


def _ssh(args: argparse.Namespace) -> dict[str, str]:
    """--ssh, as {name: bottle}: a bottle alone is reached by its own name."""
    grants: dict[str, str] = {}
    for given in args.ssh:
        name, equals, bottle = given.partition("=")
        if not equals:
            bottle = name
        if not name or not bottle:
            raise BottleError(f"invalid --ssh {given!r}: use [NAME=]BOTTLE, e.g. build or sidecar.host=build")
        if name in grants:
            raise BottleError(f"--ssh {name} given twice")
        grants[name] = bottle
    return grants


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
        table = [("NAME", "STATE", "RUNNER", "MEMORY")]
        table += [(row["name"], row["state"], row.get("runner", "local"), row.get("memory") or "-") for row in rows]
        _table(table)
        return 0
    for n, row in enumerate(rows):
        print(("\n" if n else "") + f"{row['name']}  {row['state']}")
        if row["state"] == "missing":
            continue
        if row.get("runner", "local") != "local":
            print(f"  runner    {row['runner']}")
        for name, bottle in row.get("ssh", {}).items():
            print(f"  ssh       {name} -> {bottle}")
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


def _runner_add(args: argparse.Namespace) -> int:
    from bottle import runners

    def ask(question: str) -> bool:
        if not sys.stdin.isatty():
            return False
        return input(f"{question} [Y/n] ").strip().lower() in ("", "y", "yes")

    check = runners.add(args.name, args.ssh, ask)
    print(f"Added runner {args.name} ({args.ssh}): {check.os} {check.arch}")
    print(f"Make a bottle there with: bottle new --runner {args.name} ...")
    return 0


def _runner_status(args: argparse.Namespace) -> int:
    import json

    from bottle import runners

    rows = runners.status(args.names)
    if args.json:
        print(json.dumps(rows, indent=2))
        return 0
    table = [("NAME", "SSH", "STATE", "BOTTLES")]
    for row in rows:
        if row.get("state") == "missing":
            state = "missing"
        elif row.get("detached"):
            state = "detached"
        elif not row["reachable"]:
            state = "unreachable"
        elif row.get("missing"):
            state = "needs " + ", ".join(row["missing"])
        else:
            state = f"ready{' ' + row['os'] + ' ' + row['arch'] if row.get('os') else ''}"
        table.append((row["name"], row.get("ssh") or "-", state, ", ".join(row["bottles"]) or "-"))
    _table(table)
    return 0


def _runner_stop(args: argparse.Namespace) -> int:
    from bottle import bottles, runners

    if runners.is_detached(args.name):
        print(f"Nothing was running on {args.name}: it's detached")
        return 0
    runners.get(args.name)  # one there is
    stopped, unreachable = bottles.stop_all(args.name)
    for name in stopped:
        print(f"Stopped {name}")
    if unreachable:
        raise BottleError(unreachable[args.name])
    if not stopped:
        print(f"Nothing was running on {args.name}")
    return 0


def _runner_detach(args: argparse.Namespace) -> int:
    from bottle import runners

    if runners.is_detached(args.name):
        print(f"Runner {args.name} is detached already: it's safe to turn off")
        return 0
    for name in runners.detach(args.name, args.offline):
        print(f"Stopped {name}")
    if args.offline:
        print(f"Runner {args.name} detached, unasked: nothing was stopped or checked")
    else:
        print(f"Runner {args.name} detached: nothing runs on it, so it's safe to turn off.")
    print(f"Its bottles keep their disks there. When it's back, using one attaches it again, "
          f"or `bottle runner attach {args.name}`.")
    return 0


def _runner_attach(args: argparse.Namespace) -> int:
    from bottle import runners

    stopped = runners.attach(args.name)
    print(f"Runner {args.name} attached")
    if stopped:
        print(f"Running when it was detached: {', '.join(stopped)}. Start one with `bottle start NAME`.")
    return 0


def _runner_remove(args: argparse.Namespace) -> int:
    from bottle import runners

    runners.remove(args.name)
    print(f"Removed runner {args.name}")
    return 0


def _delete(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.delete(args.name)
    print(f"Deleted {args.name}")
    return 0


def _build(args: argparse.Namespace) -> int:
    from bottle import features, images, runners

    runner = runners.get(args.runner)
    where = "" if runner.name == runners.LOCAL else f" on runner {runner.name}"
    print(f"Built {features.build(images.BASE, args.feature, args.no_cache, runner)}{where}")
    return 0


def _exec(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.exec_(args.name, args.command)  # replaces this process


def _cp(args: argparse.Namespace) -> int:
    from bottle import bottles

    target, left_out = bottles.copy(bottles.CopySide.parse(args.src), bottles.CopySide.parse(args.dst))
    for why in left_out:
        print(f"bottle: left out {why}", file=sys.stderr)
    print(f"Copied to {target}")
    return 0


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

    stopped, unreachable = bottles.stop_all()
    from bottle import daemon

    daemon_was_running = daemon.stop()
    for name in stopped:
        print(f"Stopped {name}")
    for runner, why in unreachable.items():
        print(f"Left the bottles on runner {runner} as they are: {why}", file=sys.stderr)
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
