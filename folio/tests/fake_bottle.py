#!/usr/bin/env python3
"""A stand-in for bottle's CLI, for folio's tests: $FOLIO_BOTTLE points here.

It keeps its bottles in $FAKE_BOTTLE_STATE, a JSON file, and logs every call
there. A bottle's /workspace is a real clone of its --git-repo, beside the
state file, and `exec` runs commands in it, with /workspace rewritten; tmux is
pretended, by keeping each bottle's sessions in the state. Failures look like
bottle's: `bottle: error: ...` on stderr, exit 1.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

STATE = Path(os.environ["FAKE_BOTTLE_STATE"])


def load() -> dict:
    return json.loads(STATE.read_text()) if STATE.exists() else {"bottles": {}, "calls": []}


def save(state: dict) -> None:
    STATE.write_text(json.dumps(state, indent=2))


def fail(message: str) -> None:
    print(f"bottle: error: {message}", file=sys.stderr)
    sys.exit(1)


def option(argv: list[str], name: str, many: bool = False):
    values = [argv[i + 1] for i, arg in enumerate(argv) if arg == name]
    return values if many else (values[0] if values else None)


def main(argv: list[str]) -> int:
    state = load()
    state["calls"].append(argv)
    save(state)
    bottles = state["bottles"]
    match argv:
        case ["spec", "validate", "--json", path]:
            spec = json.loads(Path(path).read_text())
            for feature in spec.get("features", []):
                if feature.split(":")[0] not in ("tools", "claude", "jvm", "python"):
                    fail(f"no feature named {feature.split(':')[0]!r}")
            print(json.dumps({"features": sorted(spec.get("features", [])), "memory": spec.get("memory", "all")}))
        case ["new", *rest]:
            name = option(rest, "--name")
            if name in bottles:
                fail(f"a bottle named {name!r} already exists")
            if os.environ.get("FAKE_BOTTLE_FAIL_NEW"):
                fail("creating failed")
            workspace = STATE.parent / "bottles" / name / "workspace"
            subprocess.run(["git", "clone", "-q", "--shared", option(rest, "--git-repo"), str(workspace)], check=True)
            if option(rest, "--checkout"):
                subprocess.run(["git", "-C", str(workspace), "checkout", "-q", option(rest, "--checkout")], check=True)
            bottles[name] = {
                "state": "running", "spec": json.loads(Path(option(rest, "--spec")).read_text()),
                "git_repo": option(rest, "--git-repo"), "remotes": option(rest, "--git-remote", many=True),
                "checkout": option(rest, "--checkout"), "context": Path(option(rest, "--context")).read_text(),
                "workspace": str(workspace), "sessions": {},
            }
            save(state)
            print(f"Created bottle '{name}'")
        case ["status", "--json", *names]:
            print(json.dumps([
                {"name": n, "state": bottles[n]["state"], "features": bottles[n]["spec"]["features"]}
                if n in bottles else {"name": n, "state": "missing"} for n in names
            ]))
        case ["start", name]:
            bottles[name]["state"] = "running"
            save(state)
        case ["delete", name]:
            del bottles[name]
            save(state)
        case ["exec", name, "--", *command]:
            bottle = bottles[name]
            bottle["state"] = "running"  # as bottle exec does: it starts a stopped bottle
            match command:
                case ["tmux", "has-session", "-t", target]:
                    return 0 if target.removeprefix("=") in bottle["sessions"] else 1
                case ["tmux", "new-session", "-d", "-s", session, "-c", _, what]:
                    bottle["sessions"][session] = what
                    save(state)
                case ["tmux", "attach", *_]:
                    pass  # attached; the call log is the evidence
                case _:
                    rewritten = [arg.replace("/workspace", bottle["workspace"]) for arg in command]
                    save(state)
                    return subprocess.run(rewritten).returncode
            save(state)
        case _:
            fail(f"the fake doesn't know {argv}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
