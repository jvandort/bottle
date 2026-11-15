#!/usr/bin/env python3
"""A stand-in for bottle-cli, for tests that drive folio's own bottles (no $FOLIO_BOTTLE): $BOTTLE_CLI points here.

Its machines, images and every call are in $FAKE_CLI_STATE, a JSON file. A machine is only a record: what it was
made with (its arguments, environment, mounts, labels and network policy) and its state. An exec is recorded, with
what it was given on stdin, and answered as a machine that meets the bottle contract would: the checks pass (or
name what $FAKE_CLI_MISSING says is missing), a key script says a key, and anything else runs here, in a directory
of the machine's own, with its home there. An image build records its recipe and policy, and the image is then
built, with the recipe's labels.

$FAKE_CLI_FAIL makes steps fail (a comma-separated list), as bottle-cli would: create, configure, start, delete, build, or an exec's
contract, hosts, mkdir or workspace. Failures look like bottle-cli's: `bottle-cli: ...` on stderr, exit 1.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

STATE = Path(os.environ["FAKE_CLI_STATE"])


def load() -> dict:
    state = json.loads(STATE.read_text()) if STATE.exists() else {}
    for key, empty in (("machines", {}), ("images", []), ("calls", []), ("execs", []), ("builds", [])):
        state.setdefault(key, empty)
    return state


def save(state: dict) -> None:
    STATE.write_text(json.dumps(state, indent=2))


def fail(message: str) -> None:
    print(f"bottle-cli: {message}", file=sys.stderr)
    sys.exit(1)


def failing(step: str) -> bool:
    return step in os.environ.get("FAKE_CLI_FAIL", "").split(",")


def options(args: list[str], takes: set[str]) -> tuple[dict[str, list[str]], list[str]]:
    """Each option in `takes` (one that takes a value), its values; the rest, positional."""
    given: dict[str, list[str]] = {}
    rest = []
    i = 0
    while i < len(args):
        if args[i] in takes:
            given.setdefault(args[i], []).append(args[i + 1])
            i += 2
        else:
            rest.append(args[i])
            i += 1
    return given, rest


def step_of(argv: list[str]) -> str:
    """Which of a bottle's setup steps an exec is, by what it runs."""
    script = argv[-1] if argv[:2] == ["sh", "-c"] else ""
    if script.startswith("# bottle contract"):
        return "contract"
    if "/etc/hosts" in script:
        return "hosts"
    if script.startswith("install -d -o"):
        return "mkdir"
    if "safe.directory" in script or "BOTTLE.md" in script:
        return "workspace"
    return ""


def main(argv: list[str]) -> int:
    state = load()
    state["calls"].append(argv)
    save(state)
    machines = state["machines"]
    match argv:
        case ["info", "--json"]:
            print(json.dumps({"memory_mib": int(os.environ.get("FAKE_CLI_MEMORY", "16384")), "cpus": 8,
                              "home": str(STATE.parent / "bottle-home")}))
        case ["images", "--json"]:
            print(json.dumps(state["images"]))
        case ["image", "build", recipe_path, "--tag", tag, "--network", network_path]:
            if failing("build"):
                fail("image build failed")
            recipe = json.loads(Path(recipe_path).read_text())
            state["builds"].append({"tag": tag, "recipe": recipe, "network": json.loads(Path(network_path).read_text())})
            state["images"] = [i for i in state["images"] if i["tag"] != tag]
            stage = f"stage-{len(state['builds'])}"
            state["images"].append({"tag": tag, "stage": stage, "disks": [f"{stage}.ext4"],
                                    "labels": recipe.get("labels", {})})
            save(state)
            print(f"/stages/{stage}.ext4")
        case ["image", "rm", tag]:
            state["images"] = [i for i in state["images"] if i["tag"] != tag]
            save(state)
        case ["gc"]:
            pass
        case ["create", name, *rest]:
            if failing("create"):
                fail("create failed")
            if name in machines:
                fail(f"machine {name} already exists")
            given, _ = options(rest, {"--image", "--cpus", "--mem", "--storage", "--user", "--workdir", "--label",
                                      "-v", "-e", "--network"})
            network = json.loads(sys.stdin.read()) if given.get("--network") == ["-"] else None
            single = lambda key: given[key][0] if key in given else None
            machines[name] = {
                "state": "created", "image": single("--image"), "cpus": single("--cpus"), "mem": single("--mem"),
                "storage": single("--storage"), "user": single("--user"), "workdir": single("--workdir"),
                "labels": dict(label.split("=", 1) for label in given.get("--label", [])),
                "mounts": given.get("-v", []), "env": dict(e.split("=", 1) for e in given.get("-e", [])),
                "network": network, "started": 0,
            }
            save(state)
        case ["configure", name, "--network", "-"]:
            if failing("configure"):
                fail("configure failed")
            if name not in machines:
                fail(f"no machine {name}")
            machines[name]["network"] = json.loads(sys.stdin.read())
            machines[name]["configured"] = machines[name].get("configured", 0) + 1
            save(state)
        case ["start", name]:
            if failing("start"):
                fail("start failed")
            if name not in machines:
                fail(f"no machine {name}")
            machines[name]["state"] = "running"
            machines[name]["started"] += 1
            save(state)
        case ["stop", name]:
            if name not in machines:
                fail(f"no machine {name}")
            machines[name]["state"] = "stopped"
            save(state)
        case ["delete", name, "-f"]:
            if failing("delete"):
                fail("machine busy")
            machines.pop(name, None)
            save(state)
        case ["status", name, "--json"]:
            if name not in machines:
                fail(f"no machine {name}")
            print(json.dumps({"name": name, "state": machines[name]["state"]}))
        case ["ls", "--json"]:
            print(json.dumps([{"name": n, "state": m["state"]} for n, m in sorted(machines.items())]))
        case ["exec", name, *rest]:
            given, positional = options(rest, {"-u", "-w", "-e"})
            dash = positional.index("--")
            tty, command = "-t" in positional[:dash], positional[dash + 1:]
            if name not in machines:
                fail(f"no machine {name}")
            if machines[name]["state"] != "running":
                fail(f"{name} isn't running")
            stdin = "" if tty else sys.stdin.read()
            step = step_of(command)
            state["execs"].append({"machine": name, "user": (given.get("-u") or [None])[0],
                                   "workdir": (given.get("-w") or [None])[0], "tty": tty, "argv": command,
                                   "stdin": stdin, "step": step})
            save(state)
            if step and failing(step):
                fail(f"{step} failed")
            if step == "contract":
                missing = os.environ.get("FAKE_CLI_MISSING")
                if missing:
                    print(missing)
                return 0
            if step:
                return 0
            script = command[-1] if command[:2] == ["sh", "-c"] else ""
            if "ssh_host_ed25519_key" in script:
                print(f"ssh-ed25519 HOSTKEY-OF-{name}")
                return 0
            if "id_ed25519" in script:
                print(f"ssh-ed25519 KEY-OF-{name}")
                return 0
            if "authorized_keys" in script or "known_hosts" in script:
                return 0
            here = STATE.parent / "machines" / name
            (here / "home").mkdir(parents=True, exist_ok=True)
            return subprocess.run(command, cwd=here, input=stdin, text=True,
                                  env={**os.environ, "HOME": str(here / "home")}).returncode
        case ["cp", src, dst]:
            print(dst)
        case _:
            fail(f"the fake doesn't know {argv}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
