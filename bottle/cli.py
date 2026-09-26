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

    build = commands.add_parser(
        "build",
        help="build a bottle image",
        description="Build the bottle image defined in containers/IMAGE.",
    )
    build.add_argument("image", help="image to build, e.g. base")
    build.add_argument("--no-cache", action="store_true", help="rebuild every step")
    build.set_defaults(run=_build, parser=build)

    egress = commands.add_parser(
        "egress",
        help="run a bottle's egress proxy (normally started by bottle itself)",
        description="Run an HTTP proxy that makes connections on the host's behalf. "
        "Public destinations are allowed; private ones only via --allow hostnames.",
    )
    egress.add_argument("name", help="the bottle this proxy serves, used in logs")
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


def _build(args: argparse.Namespace) -> int:
    import logging

    from bottle import images

    # Show the proxy's denials and failures; they explain most network errors in a build.
    logging.basicConfig(level=logging.WARNING, format="bottle: %(message)s")
    print(f"Built {images.build(args.image, args.no_cache)}")
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
