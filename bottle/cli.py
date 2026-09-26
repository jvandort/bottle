import argparse
import sys
from pathlib import Path

from bottle import repos
from bottle.errors import BottleError


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="bottle", description="Sandboxed Linux environments for agents.")
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")

    wrap = commands.add_parser(
        "wrap",
        help="register a local git repo with bottle",
        usage="bottle wrap [NAME] PATH",
        description="Register a local git repo so bottles can be created from it. "
        "NAME defaults to the origin remote's repo name, else the directory name.",
    )
    wrap.add_argument("args", nargs="+", metavar="[NAME] PATH")
    wrap.set_defaults(run=_wrap, parser=wrap)

    args = parser.parse_args(argv)
    try:
        return args.run(args)
    except BottleError as e:
        print(f"bottle: error: {e}", file=sys.stderr)
        return 1


def _wrap(args: argparse.Namespace) -> int:
    match args.args:
        case [path]:
            name = None
        case [name, path]:
            pass
        case _:
            args.parser.error("takes [NAME] PATH")

    result = repos.wrap(Path(path).expanduser(), name)
    status = "Wrapped" if result.created else "Already wrapped"
    print(f"{status} {result.repo.name}: {result.repo.path}")
    return 0
