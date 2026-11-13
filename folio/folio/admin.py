"""What folio does with bottle's parts themselves, not a thread's: the credentials bottles get, the runners they run
on, the bottles no thread has, and stopping it all. Each is bottle's (bottle/auth.py, runners.py, bottles.py), in
this process, as the engine's are.

A machine's own commands, by its name, are bottle-cli's: `bottle start|stop|shell|exec|cp MACHINE`.
"""

import argparse
import getpass
import json
import sys

from . import engine, repos, threads
from .errors import FolioError
from bottle import auth, bottles, daemon, features, runners  # engine has put bottle's package where it's found


def add(commands: argparse._SubParsersAction) -> None:
    """These commands, among folio's."""
    auth_ = commands.add_parser("auth", help="credentials: set once; bottles get those their features need")
    auth_commands = auth_.add_subparsers(required=True)
    command = auth_commands.add_parser("list", help="list credentials features declare, and which are set")
    command.set_defaults(run=_auth_list)
    command = auth_commands.add_parser(
        "login", help="log in, with the credential's own login flow",
        description="Log in and store the credential in the Keychain. Runs the declaring feature's login "
        "command in a throwaway bottle, attached to this terminal, and captures the credential it prints.")
    command.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    command.set_defaults(run=_auth_login)
    command = auth_commands.add_parser("set", help="store a credential read from stdin",
                                       description="Store a credential read from stdin.")
    command.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    command.set_defaults(run=_auth_set)
    command = auth_commands.add_parser("logout", help="remove a stored credential")
    command.add_argument("name", metavar="CREDENTIAL", help="the credential, e.g. claude")
    command.set_defaults(run=_auth_logout)

    runner = commands.add_parser("runner", help="the machines bottles run on: this one, and others over ssh")
    runner_commands = runner.add_subparsers(required=True)
    command = runner_commands.add_parser(
        "add", help="add a machine to run bottles on, over ssh",
        description="Add a Linux machine to run bottles on, reached over ssh as you reach it. Checks what it has, "
        "and copies bottle-cli over, built here for its architecture (on a Mac, in a Linux VM).")
    command.add_argument("name", metavar="RUNNER", help="its name, as `folio configure --runner` takes it")
    command.add_argument("--ssh", required=True, metavar="DESTINATION",
                         help="where ssh reaches it: a Host from ~/.ssh/config, or user@host")
    command.set_defaults(run=_runner_add)
    command = runner_commands.add_parser(
        "list", aliases=["ls", "status"], help="show runners: reachable or not, what they're missing, their bottles")
    command.add_argument("names", nargs="*", metavar="RUNNER", help="the runners to show (default: all)")
    command.add_argument("--json", action="store_true", help="print JSON: one object per runner")
    command.set_defaults(run=_runner_status)
    command = runner_commands.add_parser(
        "stop", help="stop every bottle on a runner, to shut the machine down",
        description="Stop every bottle on a runner, so the machine can be shut down. Their disks stay on it, "
        "and each starts again when it's next used.")
    command.add_argument("name", metavar="RUNNER", help="the runner whose bottles to stop")
    command.set_defaults(run=_runner_stop)
    command = runner_commands.add_parser(
        "detach", help="stop everything on a runner and let it go, to turn it off",
        description="Stop every bottle on a runner (asking first), check nothing else runs there, and let it go: "
        "then it's safe to turn off. Until it's attached again it isn't asked anything, and its bottles show as "
        "detached. Using one of them attaches it again, if it's back; so does `folio runner attach`.")
    command.add_argument("name", metavar="RUNNER", help="the runner to detach")
    command.add_argument("--offline", action="store_true",
                         help="it's off already: mark it detached without asking it anything")
    command.set_defaults(run=_runner_detach)
    command = runner_commands.add_parser(
        "attach", help="attach a detached runner again, once it's back",
        description="Attach a detached runner again, once it answers, and name the bottles detaching it stopped.")
    command.add_argument("name", metavar="RUNNER", help="the runner to attach")
    command.set_defaults(run=_runner_attach)
    command = runner_commands.add_parser(
        "remove", aliases=["rm"], help="forget a runner with no bottles",
        description="Forget a runner with no bottles on it. Nothing on the machine is touched.")
    command.add_argument("name", metavar="RUNNER", help="the runner to forget")
    command.set_defaults(run=_runner_remove)

    bottle = commands.add_parser(
        "bottle", help="bottles themselves: each thread's, its sidecar's, and any no thread has",
        description="Bottles themselves, wherever their threads are. One no thread has (made before folio kept "
        "them, or left by one) is removed here; a thread's goes with its thread.")
    bottle_commands = bottle.add_subparsers(required=True)
    command = bottle_commands.add_parser("list", aliases=["ls"], help="every bottle, its thread and its state")
    command.add_argument("--json", action="store_true", help="print JSON: one object per bottle")
    command.set_defaults(run=_bottle_list)
    command = bottle_commands.add_parser("remove", aliases=["rm"], help="delete a bottle no thread has")
    command.add_argument("name", metavar="BOTTLE", help="the bottle")
    command.set_defaults(run=_bottle_remove)

    command = commands.add_parser("shutdown", help="stop every bottle, and folio serve",
                                  description="Stop every running bottle, then folio serve.")
    command.set_defaults(run=_shutdown)


def _table(rows: list[tuple[str, ...]]) -> None:
    from .cli import _table   # folio's way with rows; cli imports this module

    _table(rows)


def _auth_list(args: argparse.Namespace) -> int:
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
    status = "set" if is_set else "not set"
    if declared.credential.properties:
        values = features.load(declared.feature).values
        path = features.OPTION_REFERENCE.sub(lambda m: str(values[m.group(1)]), declared.credential.properties)
        status = f"{path}, else {status}"
    return status


def _auth_login(args: argparse.Namespace) -> int:
    auth.login(args.name)
    print(f"Logged in: {args.name}. Bottles with it get it when they next start.")
    return 0


def _auth_set(args: argparse.Namespace) -> int:
    value = getpass.getpass(f"{args.name}: ") if sys.stdin.isatty() else sys.stdin.read()
    auth.set_value(args.name, value)
    print(f"Stored {args.name}. Bottles with it get it when they next start.")
    return 0


def _auth_logout(args: argparse.Namespace) -> int:
    print(f"Removed {args.name}" if auth.logout(args.name) else f"{args.name} wasn't set")
    return 0


def _runner_add(args: argparse.Namespace) -> int:
    check = runners.add(args.name, args.ssh)
    print(f"Added runner {args.name} ({args.ssh}): {check.os} {check.arch}")
    print(f"Make a repo's bottles there with: folio configure --runner {args.name}")
    return 0


def _runner_status(args: argparse.Namespace) -> int:
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
    if runners.is_detached(args.name):
        print(f"Nothing was running on {args.name}: it's detached")
        return 0
    runners.get(args.name)  # one there is
    stopped, unreachable = bottles.stop_all(args.name)
    for name in stopped:
        print(f"Stopped {name}")
    if unreachable:
        raise FolioError(unreachable[args.name])
    if not stopped:
        print(f"Nothing was running on {args.name}")
    return 0


def _runner_detach(args: argparse.Namespace) -> int:
    if runners.is_detached(args.name):
        print(f"Runner {args.name} is detached already: it's safe to turn off")
        return 0

    def ask(running: list[str]) -> bool:
        if not sys.stdin.isatty():
            return False
        print(f"Running on {args.name}: {', '.join(running)}. Stopping one ends whatever runs in it, its agent's "
              "work included: tell them to wrap up first, if they're busy.")
        return input("Stop them now? [y/N] ").strip().lower() in ("y", "yes")

    for name in runners.detach(args.name, args.offline, ask):
        print(f"Stopped {name}")
    if args.offline:
        print(f"Runner {args.name} detached, unasked: nothing was stopped or checked")
    else:
        print(f"Runner {args.name} detached: nothing runs on it, so it's safe to turn off.")
    print(f"Its bottles keep their disks there. When it's back, using one attaches it again, "
          f"or `folio runner attach {args.name}`.")
    return 0


def _runner_attach(args: argparse.Namespace) -> int:
    stopped = runners.attach(args.name)
    print(f"Runner {args.name} attached")
    if stopped:
        print(f"Running when it was detached: {', '.join(stopped)}. Each starts when it's next used.")
    return 0


def _runner_remove(args: argparse.Namespace) -> int:
    runners.remove(args.name)
    print(f"Removed runner {args.name}")
    return 0


def _owners() -> dict[str, str]:
    """Each thread's bottles' names, its own and its sidecar's, to the thread, as REPO/THREAD."""
    owners = {}
    for repo in repos.known():
        for thread in threads.all_(repo):
            owners[thread.bottle] = owners[thread.sidecar_bottle] = f"{repo.name}/{thread.name}"
    return owners


def _bottle_list(args: argparse.Namespace) -> int:
    owners = _owners()
    rows = [{**row, "thread": owners.get(row["name"])} for row in bottles.status()]
    if args.json:
        print(json.dumps(rows, indent=2))
        return 0
    table = [("BOTTLE", "THREAD", "STATE", "RUNNER", "MACHINE")]
    table += [(row["name"], row["thread"] or "-", row["state"], row.get("runner", "local"), row.get("machine", "-"))
              for row in rows]
    _table(table)
    return 0


def _bottle_remove(args: argparse.Namespace) -> int:
    owner = _owners().get(args.name)
    if owner is not None and engine.status([args.name])[args.name]["state"] != "missing":
        thread = owner.split("/", 1)[1]
        raise FolioError(f"{args.name} is thread {owner}'s: it goes with the thread (`folio delete {thread}`, or "
                         f"`folio pause {thread}`)")
    engine.delete(args.name)
    print(f"Deleted {args.name}")
    return 0


def _shutdown(args: argparse.Namespace) -> int:
    stopped, unreachable = bottles.stop_all()
    was_running = daemon.stop()
    for name in stopped:
        print(f"Stopped {name}")
    for runner, why in unreachable.items():
        print(f"Left the bottles on runner {runner} as they are: {why}", file=sys.stderr)
    if was_running:
        print("Stopped folio serve")
    if not stopped and not was_running:
        print("Nothing was running")
    return 0
