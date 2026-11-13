#!/usr/bin/env python3
"""A stand-in for bottle's CLI, for folio's tests: $FOLIO_BOTTLE points here.

It keeps its bottles in $FAKE_BOTTLE_STATE, a JSON file, and logs every call
there. Each --mount is a real clone of its repo, beside the state file, and
`exec` runs commands with every mount's path rewritten to its clone (the first
mount's is the bottle's "workspace") and $HOME a home of the bottle's own, which starts with the settings.json an image
would give it; shpool is pretended, by keeping each bottle's sessions in the
state. Failures look like
bottle's: `bottle: error: ...` on stderr, exit 1.
"""

import json
import os
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

STATE = Path(os.environ["FAKE_BOTTLE_STATE"])
# A pretended session's pid: above Linux's highest, so no process has it. A
# session in a bottle's "exited" is one whose program has, and that shpool lists
# all the same, with a pid that kill -0 doesn't find.
NO_PID, EXITED_PID = 4194305, 4194306


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
    # A terminal, asked for: the call log has it.
    if argv[:2] == ["exec", "-t"]:
        argv = ["exec", *argv[2:]]
    bottles = state["bottles"]
    # A runner that's off (detached): its bottles show as such, and can't be made, started or deleted.
    off = os.environ.get("FAKE_BOTTLE_OFF")
    if off and argv[0] in ("new", "start", "delete"):
        on = option(argv, "--runner") if argv[0] == "new" else bottles.get(argv[-1], {}).get("runner")
        if on == off:
            fail(f"runner {off} is detached, and doesn't answer yet: turn it on, then try again")
    match argv:
        case ["spec", "validate", "--json", path]:
            spec = json.loads(Path(path).read_text())
            if "memory" not in spec:
                fail("a spec needs its memory")
            for feature in spec.get("features", []):
                if feature.split(":")[0] not in ("tools", "claude", "jvm", "python"):
                    fail(f"no feature named {feature.split(':')[0]!r}")
            canonical = {"features": sorted(spec.get("features", [])), "memory": spec["memory"]}
            print(json.dumps(canonical | {key: spec[key] for key in ("disk", "permissions", "labels") if key in spec}))
        case ["new", *rest]:
            name = option(rest, "--name")
            if name in bottles:
                fail(f"a bottle named {name!r} already exists")
            if os.environ.get("FAKE_BOTTLE_FAIL_NEW") in ("1", name):   # every bottle, or the one named
                fail("creating failed")
            ssh = dict(g.partition("=")[::2] for g in option(rest, "--ssh", many=True))
            for host, target in ssh.items():
                if target not in bottles:
                    fail(f"--ssh {host}={target}: no bottle named {target!r}")
            root = STATE.parent / "bottles" / name
            (root / "home" / ".claude").mkdir(parents=True)
            (root / "home" / ".claude" / "settings.json").write_text('{"from": "the image"}')
            mounts = []
            for given in option(rest, "--mount", many=True):
                path, _, repo = given.partition("=")
                checkout = next((c.partition("=")[2] for c in option(rest, "--checkout", many=True)
                                 if c.partition("=")[0] == path), None)
                remotes = [g.partition(":")[2] for g in option(rest, "--git-remote", many=True) if g.partition(":")[0] == path]
                clone = root / "mounts" / path.strip("/")
                subprocess.run(["git", "clone", "-q", "--shared", repo, str(clone)], check=True)
                if checkout:
                    subprocess.run(["git", "-C", str(clone), "checkout", "-q", checkout], check=True)
                mounts.append({"path": path, "repo": repo, "checkout": checkout, "remotes": remotes, "dir": str(clone)})
            bottles[name] = {
                "state": "running", "spec": json.loads(Path(option(rest, "--spec")).read_text()),
                "mounts": mounts, "context": Path(option(rest, "--context")).read_text(),
                "workspace": mounts[0]["dir"] if mounts else str(root / "home"), "home": str(root / "home"),
                "sessions": {}, "runner": option(rest, "--runner") or "local", "ssh": ssh, "last_active": time.time(),
            }
            save(state)
            print(f"Created bottle '{name}'")
        case ["status", "--json", *names]:
            print(json.dumps([
                # Each mount's objects as its clone has them (--shared: the repo's own), and its remotes' URLs.
                {"name": n, "features": bottles[n]["spec"]["features"], "ssh": bottles[n].get("ssh", {}),
                 "last_active": bottles[n].get("last_active", 0.0),
                 "state": "detached" if bottles[n].get("runner") == off else bottles[n]["state"],
                 "runner": bottles[n].get("runner", "local"),
                 "mounts": [{"path": m["path"], "objects": str(Path(m["repo"]) / ".git" / "objects"),
                             "urls": {r.partition(":")[0]: f"http://bottle.host/git{m['path']}/{r.partition(':')[0]}"
                                      for r in m["remotes"]}} for m in bottles[n]["mounts"]]}
                if n in bottles else {"name": n, "state": "missing"} for n in names
            ]))
        case ["touch", name]:
            bottles[name]["last_active"] = time.time()
            save(state)
        case ["start", name]:
            bottles[name]["state"] = "running"
            save(state)
        case ["delete", name]:
            if os.environ.get("FAKE_BOTTLE_FAIL_DELETE") == name:
                fail(f"couldn't fully delete {name}")
            shutil.rmtree(Path(bottles.pop(name)["home"]).parent)
            save(state)
        case ["exec", name, "--", *command]:
            bottle = bottles[name]
            bottle["state"] = "running"  # as bottle exec does: it starts a stopped bottle
            match command:
                case ["shpool", "list", "--json"]:
                    exited = bottle.get("exited", [])
                    print(json.dumps({"sessions": [{"name": n, "shell_pid": EXITED_PID if n in exited else NO_PID}
                                                   for n in bottle["sessions"]]}))
                case ["env", _, "shpool", "attach", "--background", "--dir", _, "--cmd", what, session]:
                    bottle["sessions"][session] = shlex.split(what)[2]
                case ["kill", "-0", pid]:
                    return 0 if pid == str(NO_PID) else 1
                case ["shpool", "kill", session]:
                    del bottle["sessions"][session]
                    bottle["exited"] = [n for n in bottle.get("exited", []) if n != session]
                case ["sh", "-c", _, "attach", _, _, _]:
                    pass  # attached; the call log is the evidence
                case ["kill", "-HUP", *_]:
                    bottle["sessions"] = {}
                case _:
                    rewritten = command
                    for mount in sorted(bottle["mounts"], key=lambda m: len(m["path"]), reverse=True):
                        rewritten = [arg.replace(mount["path"], mount["dir"]) for arg in rewritten]
                    save(state)
                    home = bottle["home"]
                    return subprocess.run(rewritten, env={**os.environ, "HOME": home}).returncode
            save(state)
        case _:
            fail(f"the fake doesn't know {argv}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
