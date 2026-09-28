import argparse
import sys
from pathlib import Path

from bottle import repos
from bottle.errors import BottleError


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="bottle", description="Sandboxed Linux environments for agents.")
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")

    repo = commands.add_parser("repo", help="manage the repos bottles are created from")
    repo_commands = repo.add_subparsers(dest="repo_command", required=True, metavar="COMMAND")
    feature_help = "a default feature for its bottles, optionally with options, e.g. jvm:version=17 (repeatable)"
    repo_add = repo_commands.add_parser(
        "add",
        help="register a local git repo",
        usage="bottle repo add [NAME] PATH [--feature FEATURE]...",
        description="Register a local git repo so bottles can be created from it. "
        "NAME defaults to the origin remote's repo name, else the directory name.",
    )
    repo_add.add_argument("args", nargs="+", metavar="[NAME] PATH")
    repo_add.add_argument("--feature", action="append", default=[], metavar="FEATURE", help=feature_help)
    repo_add.set_defaults(run=_repo_add, parser=repo_add)
    repo_list = repo_commands.add_parser("list", help="list repos and their default features")
    repo_list.set_defaults(run=_repo_list, parser=repo_list)
    repo_set = repo_commands.add_parser(
        "set",
        help="replace a repo's settings",
        description="Replace all of a repo's settings: its default features become exactly those given "
        "(none, if none are given). Existing bottles get them when reset.",
    )
    repo_set.add_argument("name", metavar="REPO", help="the repo to change")
    repo_set.add_argument("--feature", action="append", default=[], metavar="FEATURE", help=feature_help)
    repo_set.set_defaults(run=_repo_set, parser=repo_set)

    auth = commands.add_parser("auth", help="log in once; bottles get the credentials their features need")
    auth_commands = auth.add_subparsers(dest="auth_command", required=True, metavar="COMMAND")
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

    new = commands.add_parser(
        "new",
        help="create a bottle from a repo",
        description="Create a bottle: REPO's branch checked out at /workspace, in its own VM. "
        "NAME defaults to the repo's name, then REPO-2, REPO-3, ...",
    )
    new.add_argument("repo", metavar="REPO", help="the repo to create a bottle from (added with `bottle repo add`)")
    new.add_argument(
        "--feature", action="append", default=[], metavar="FEATURE",
        help="a feature to add to the repo's defaults, optionally with options, e.g. jvm:version=17 "
        "(repeatable); given for a default feature, it replaces that feature's options",
    )
    new.add_argument(
        "--branch",
        help="branch to check out (default: origin's default branch, else what the repo has checked out)",
    )
    new.add_argument("--name", help="name for the bottle")
    new.set_defaults(run=_new, parser=new)

    list_ = commands.add_parser("list", help="list bottles", description="List bottles.")
    list_.set_defaults(run=_list, parser=list_)

    shell = commands.add_parser("shell", help="open a shell in a bottle", description="Open a shell in a bottle, starting it if needed.")
    shell.add_argument("name", metavar="BOTTLE", help="the bottle to open a shell in")
    shell.set_defaults(run=_shell, parser=shell)

    git = commands.add_parser("git", help="move git work between bottles and their repos")
    git_commands = git.add_subparsers(dest="git_command", required=True, metavar="COMMAND")
    fetch = git_commands.add_parser(
        "fetch",
        help="fetch a bottle's work into its repo",
        description="Fetch a bottle's git work into its repo, as bottle-NAME/<branch>. "
        "Only adds and fast-forwards: history the bottle rewrote is refused, and nothing is deleted. "
        "With no REV: every branch, plus a detached HEAD. With a branch: just that branch. "
        "With a commit: that commit, into FETCH_HEAD.",
    )
    fetch.add_argument("name", metavar="BOTTLE", help="the bottle to fetch from")
    fetch.add_argument("rev", nargs="?", help="a branch or commit in the bottle (default: every branch)")
    fetch.add_argument(
        "-f", "--force", action="store_true", help="overwrite a branch the bottle rewrote (needs a single BRANCH)"
    )
    fetch.set_defaults(run=_fetch, parser=fetch)

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
    delete.add_argument(
        "-f", "--force", action="store_true",
        help="delete even if the bottle has unfetched commits or uncommitted changes",
    )
    delete.set_defaults(run=_delete, parser=delete)

    reset = commands.add_parser(
        "reset",
        help="delete a bottle and create it again",
        description="Delete a bottle and create it again with `bottle new`'s original arguments, in one "
        "step: a fresh, running VM with the repo's current default features (plus any the bottle was "
        "created with), and /workspace at the latest commit of its branch.",
    )
    reset.add_argument("name", metavar="BOTTLE", help="the bottle to reset")
    reset.add_argument(
        "-f", "--force", action="store_true",
        help="reset even if the bottle has unfetched commits or uncommitted changes",
    )
    reset.set_defaults(run=_reset, parser=reset)

    shutdown = commands.add_parser(
        "shutdown", help="stop every bottle and bottled", description="Stop every running bottle, then bottled."
    )
    shutdown.set_defaults(run=_shutdown, parser=shutdown)

    daemon = commands.add_parser(
        "daemon",
        help="control bottled, which gives bottles network access (started automatically when needed)",
    )
    daemon_commands = daemon.add_subparsers(dest="daemon_command", required=True, metavar="COMMAND")
    daemon_start = daemon_commands.add_parser("start", help="start bottled in the background")
    daemon_start.add_argument("--foreground", action="store_true", help="run in this process instead")
    daemon_start.set_defaults(run=_daemon_start, parser=daemon_start)
    daemon_stop = daemon_commands.add_parser(
        "stop", help="stop bottled; running bottles lose network access until it's started again"
    )
    daemon_stop.set_defaults(run=_daemon_stop, parser=daemon_stop)

    egress = commands.add_parser(
        "egress",
        help="run a bottle's egress proxy (normally started by bottle itself)",
        description="Run an HTTP proxy that makes connections on the host's behalf. "
        "Public destinations are allowed; private ones only via --allow hostnames.",
    )
    egress.add_argument("name", metavar="BOTTLE", help="the bottle this proxy serves, used in logs")
    egress.add_argument("--listen", required=True, metavar="HOST:PORT", help="address to listen on")
    egress.add_argument(
        "--allow", action="append", default=[], metavar="PATTERN",
        help="hostname pattern allowed to reach private addresses (repeatable)",
    )
    egress.set_defaults(run=_egress, parser=egress)

    args = parser.parse_args(argv)
    try:
        return args.run(args)
    except BottleError as e:
        print(f"bottle: error: {e}", file=sys.stderr)
        return 1


def _repo_add(args: argparse.Namespace) -> int:
    match args.args:
        case [path]:
            name = None
        case [name, path]:
            pass
        case _:
            args.parser.error("takes [NAME] PATH")

    result = repos.add(Path(path).expanduser(), name, args.feature)
    repo = result.repo
    if result.created:
        print(f"Created repo '{repo.name}' ({repo.path}) with features {_features(repo.features)}")
    else:
        print(f"Repo '{repo.name}' already exists ({repo.path}) with features {_features(repo.features)}")
    return 0


def _features(specs) -> str:
    return f"[{', '.join(sorted(specs))}]"


def _repo_list(args: argparse.Namespace) -> int:
    rows = [("NAME", "PATH", "FEATURES")]
    rows += [(r.name, str(r.path), " ".join(r.features) or "-") for r in sorted(repos.load().values(), key=lambda r: r.name)]
    _table(rows)
    return 0


def _repo_set(args: argparse.Namespace) -> int:
    from bottle import bottles

    repo = repos.set_settings(args.name, args.feature)
    print(f"Set repo '{repo.name}' features to {_features(repo.features)}")
    stale = [b.name for b in bottles.load().values()
             if b.repo == repo.name and set(bottles.features_on_reset(b)) != set(b.features)]
    for name in stale:
        print(f"Bottle '{name}' gets them when reset: bottle reset {name}")
    return 0


def _table(rows: list[tuple[str, ...]]) -> None:
    widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
    for row in rows:
        print("  ".join(cell.ljust(w) for cell, w in zip(row, widths)).rstrip())


def _auth_list(args: argparse.Namespace) -> int:
    from bottle import auth

    rows = [("CREDENTIAL", "FEATURE", "STATUS", "DESCRIPTION")]
    rows += [(d.credential.name, d.feature, "set" if is_set else "not set", d.credential.description)
             for d, is_set in auth.status()]
    _table(rows)
    return 0


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


def _new(args: argparse.Namespace) -> int:
    from bottle import bottles

    print(f"Creating a bottle from {args.repo}...", file=sys.stderr)
    from bottle import images

    bottle = bottles.create(args.repo, images.BASE, args.branch, args.name, args.feature)
    print(f"Created {_describe(bottle)}")
    print(f"Open a shell with: bottle shell {bottle.name}")
    return 0


def _describe(bottle) -> str:
    at = f"{bottle.branch} ({bottle.commit[:12]})" if bottle.branch else f"commit {bottle.commit[:12]}"
    return f"bottle '{bottle.name}' from repo '{bottle.repo}' at {at} with features {_features(bottle.features)}"


def _shell(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.shell(args.name)
    return 0  # not reached: shell replaces this process


def _list(args: argparse.Namespace) -> int:
    from bottle import bottles

    rows = [("NAME", "REPO", "BRANCH", "FEATURES", "STATE")]
    rows += [(b.name, b.repo, b.checkout, " ".join(sorted(b.features)) or "-", state) for b, state in bottles.list_all()]
    _table(rows)
    return 0


def _delete(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottles.delete(args.name, args.force)
    print(f"Deleted {args.name}")
    return 0


def _build(args: argparse.Namespace) -> int:
    import logging

    from bottle import features, images

    # Show the proxy's denials and failures; they explain most network errors in a build.
    logging.basicConfig(level=logging.WARNING, format="bottle: %(message)s")
    print(f"Built {features.build(images.BASE, args.feature, args.no_cache)}")
    return 0


def _fetch(args: argparse.Namespace) -> int:
    from bottle import bottles

    result = bottles.fetch(args.name, args.rev, args.force)
    if result.commit:
        print(f"Fetched {result.commit[:12]} into FETCH_HEAD; keep it with: git branch <name> {result.commit[:12]}")
    for u in result.updates:
        change = u.new[:12] if u.old is None else f"{u.old[:12]} -> {u.new[:12]}"
        print(f"  {u.kind:<8} {u.ref.removeprefix('refs/remotes/')}  {change}")
    for branch in result.gone:
        print(f"  gone     bottle-{args.name}/{branch}  (deleted in the bottle; kept here)")
    if not result.commit and not result.updates and not result.gone:
        print("Already up to date")
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


def _reset(args: argparse.Namespace) -> int:
    from bottle import bottles

    bottle = bottles.reset(args.name, args.force)
    print(f"Reset {_describe(bottle)}")
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
    import logging

    from bottle import daemon

    if not args.foreground:
        print("Started bottled" if daemon.start() else "bottled is already running")
        return 0
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(message)s")
    try:
        asyncio.run(daemon.serve())
    except KeyboardInterrupt:
        pass
    return 0


def _egress(args: argparse.Namespace) -> int:
    import asyncio
    import logging

    from bottle import egress

    host, _, port = args.listen.rpartition(":")
    if not host or not port.isdigit():
        args.parser.error(f"--listen must be HOST:PORT, got {args.listen!r}")
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(message)s")
    policy = egress.Policy(allow_private=tuple(args.allow))
    try:
        asyncio.run(egress.serve(args.name, host.strip("[]"), int(port), policy))
    except KeyboardInterrupt:
        pass
    except OSError as e:
        raise BottleError(f"cannot listen on {args.listen}: {e.strerror}") from None
    return 0
