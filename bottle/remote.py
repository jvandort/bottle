"""A remote runner's own end: its operations, run on it for a controller elsewhere.

A remote runner is a local runner operated from another machine. The
controller runs `python3 -m bottle.remote OP` there (over ssh, see
runners.RemoteRunner), with the operation's arguments as JSON on stdin, and
reads its result as JSON on stdout: {"ok": true, "result": ...}, or
{"ok": false, "error": ...} for a BottleError, which the controller raises
as its own. Each operation is the local runner's, on the machine this runs on.

Never interactive: there's no one at this end to ask. What a local runner
would ask before setting up, the controller asks its user first.
"""

import json
import os
import sys
from pathlib import Path

from bottle import runners
from bottle.link import SOCKET, served_from_json
from bottle.store import bottle_home
from bottle.errors import BottleError


def _path(value: Path | None) -> str | None:
    return str(value) if value is not None else None


def _run(op: str, args: dict):
    local = runners.get(runners.LOCAL)
    match op:
        case "archive":
            return _path(local.archive(args["tag"]))
        case "create":
            local.create(args["machine"], Path(args["archive"]),
                         [runners.Mount(Path(m["source"]), m["target"]) for m in args["mounts"]],
                         args["env"], args["memory_mib"], args["cpus"])
        case "start":
            local.start(args["machine"], args["bottle"], served_from_json(args["served"]), args["features"])
        case "release":
            local.release(args["bottle"])
        case "ensure_deciding":
            local.ensure_deciding()
        case "stop":
            local.stop(args["machine"])
        case "delete":
            local.delete(args["machine"])
        case "state":
            return local.state(args["machine"])
        case "exec":
            return local.exec_(args["machine"], args["argv"], args.get("user"), args.get("workdir"),
                               args.get("input"), args.get("env"))
        case "exec_command":
            return local.exec_command(args["machine"], args["argv"], args.get("user"), args.get("workdir"),
                                      args.get("tty", False), args.get("env"))
        case _:
            raise BottleError(f"no runner operation {op!r}")
    return None


def answer(op: str, request: str) -> str:
    """The JSON answer to `op`, given its arguments as JSON."""
    try:
        return json.dumps({"ok": True, "result": _run(op, json.loads(request or "{}"))})
    except BottleError as e:
        return json.dumps({"ok": False, "error": str(e)})


def main(argv: list[str]) -> int:
    # The smolvm bottle installed here, unless one's named: one on PATH is used only without it.
    installed = Path.home() / ".bottle" / "smolvm" / "current" / "smolvm"
    if not os.environ.get("BOTTLE_SMOLVM") and installed.exists():
        os.environ["BOTTLE_SMOLVM"] = str(installed)
    # The bottled this starts here is the runner's half alone: the controller's is reached over the link.
    os.environ["BOTTLE_LINK"] = str(bottle_home() / SOCKET)
    if len(argv) != 1:
        print("usage: python3 -m bottle.remote OP < ARGS.json", file=sys.stderr)
        return 2
    print(answer(argv[0], sys.stdin.read()))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
