"""A drone, end to end, with only ssh and bottle-cli faked: the controller's bottled, the link, a runner's bottled.

The runner is this machine, in a home of its own, reached through a fake ssh
that runs each command there and makes a forward (-R) a symlink to the
socket here. Each bottled's decider listens on a port of its own choosing
(BOTTLE_DECIDER_PORT=0), so neither meets a real one. bottle-cli there records
how a machine was started; the test is then its guest, opening a connection
through the runner's decider as bottle-cli would.
"""

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from bottle import bottles, daemon, decider, features, host, images, mirrors, prereqs, repos, runners
from bottle.host import Remote, Served
from bottle.errors import BottleError
from tests.support import GitTestCase, run

FAKE_SSH = r"""#!/usr/bin/env bash
forward=
while [ "$1" != "--" ]; do [ "$1" = -R ] && forward=$2; shift; done
shift; destination=$1; shift
mkdir -p "$FAKE_REMOTES/$destination"
if [ -n "$forward" ]; then ln -sfn "${forward#*:}" "${forward%%:*}"; exec sleep 3600; fi
exec env -u BOTTLE_HOME HOME="$FAKE_REMOTES/$destination" bash -c "$1"
"""

# bottle-cli there: a machine started is recorded, with the decider and token it was given.
FAKE_CLI = r"""#!/usr/bin/env bash
if [ "$1 $2" = "machine start" ]; then
  printf '{"args": "%s", "token": "%s"}' "$*" "$BOTTLE_DECIDER_TOKEN" > "$HOME/started.json"
fi
echo '{"state": "running"}'
"""


# bottle-cli there with machines: made, started, stopped, deleted and asked about, each a file; exec runs nothing.
STATEFUL_CLI = r"""#!/usr/bin/env bash
op="$1 $2"; shift 2
while [ $# -gt 0 ]; do [ "$1" = --name ] && name=$2; [ "$1" = -- ] && break; shift; done
machines="$HOME/machines"; mkdir -p "$machines"
case "$op" in
  "machine create") echo created > "$machines/$name" ;;
  "machine start")
    echo running > "$machines/$name"
    printf '{"args": "%s", "token": "%s"}' "$*" "$BOTTLE_DECIDER_TOKEN" > "$HOME/started.json" ;;
  "machine stop") echo stopped > "$machines/$name" ;;
  "machine delete") rm -f "$machines/$name" ;;
  "machine status")
    if [ -f "$machines/$name" ]; then printf '{"state": "%s"}' "$(cat "$machines/$name")"
    else echo "bottle-cli: no machine $name" >&2; exit 1; fi ;;
  "machine exec") cat > /dev/null; echo "$*" >> "$HOME/execs" ;;
  "machine ls")
    printf '['; sep=
    for m in "$machines"/*; do
      [ -f "$m" ] && printf '%s{"name": "%s", "state": "%s"}' "$sep" "${m##*/}" "$(cat "$m")" && sep=,
    done
    echo ']' ;;
esac
"""

# ssh to a runner that's off.
OFF_SSH = r"""#!/usr/bin/env bash
echo "ssh: connect to host box port 22: No route to host" >&2
exit 255
"""


class DroneCase(GitTestCase):
    """A controller here, and runner box: this machine, in a home of its own."""

    def setUp(self) -> None:
        super().setUp()
        short = Path(tempfile.mkdtemp(dir="/tmp"))   # macOS caps Unix socket paths at 104 bytes
        self.addCleanup(shutil.rmtree, short, ignore_errors=True)
        self.remote = short / "r" / "box"
        cli = self.remote / ".bottle" / "dist" / "bottle-cli"
        cli.parent.mkdir(parents=True)
        cli.write_text(FAKE_CLI)
        cli.chmod(0o755)
        # Built from these sources, as far as the runner's bottle can tell (see prereqs.ensure_cli).
        (cli.parent / prereqs.STAMP).write_text(prereqs.source_hash())
        fake = short / "ssh"
        fake.write_text(FAKE_SSH)
        fake.chmod(0o755)
        env = mock.patch.dict(os.environ, {
            "BOTTLE_HOME": str(short / "controller"), "BOTTLE_SSH": str(fake), "FAKE_REMOTES": str(short / "r"),
            "BOTTLE_DECIDER_PORT": "0",
        })
        env.start()
        self.addCleanup(env.stop)
        self.addCleanup(self.stop_both)
        runners.register("box", "box")

    def stop_both(self) -> None:
        daemon.stop()  # the controller's, and with it the link
        there = self.remote / ".bottle" / "bottled.sock"
        if there.exists():
            with socket.socket(socket.AF_UNIX) as s:
                s.connect(str(there))
                s.sendall(b'{"op": "shutdown"}\n')
                s.recv(100)

    def guest_connects(self, started: dict, address: str, port: int, first: bytes) -> bytes:
        """A connection from the machine's guest, as bottle-cli hands it to the decider it was given."""
        decider_address = started["args"].split("--decider ")[1].split()[0]
        ip, _, decider_port = decider_address.rpartition(":")
        with socket.create_connection((ip, int(decider_port)), timeout=10) as s:
            s.sendall(decider.MAGIC + bytes.fromhex(started["token"]) + b"m" * 16 + bytes(16) + bytes([4])
                      + port.to_bytes(2, "big") + socket.inet_aton(address) + len(first).to_bytes(2, "big") + first)
            answer = b""
            while chunk := s.recv(65536):
                answer += chunk
        return answer



class DroneTest(DroneCase):
    def test_a_drones_fetch_is_served_from_its_runners_mirror_of_the_hub(self) -> None:
        upstream = self.make_repo("upstream")
        repo = self.tmp / "repo"
        run("git", "clone", "-q", upstream, repo)
        remotes = (Remote("origin", "upstream"),)
        bottles._write({"b": bottles.Bottle("b", "i", "base", 0.0, status="ready", runner="box", mounts=(
            bottles.Mount("/workspace", str(repo), "main", run("git", "-C", repo, "rev-parse", "HEAD"), remotes),))})
        box = runners.get("box")

        objects = box.objects(repos.Repo("repo", repo))   # what its machine mounts: the mirror there, made now
        self.assertEqual(objects, self.remote / ".bottle" / "git" / f"{mirrors.key(repo / '.git')}.git" / "objects")
        new = self.commit(upstream, "since the controller last fetched")
        box.start("bottle-b", "b", (Served(repo / ".git", remotes, "workspace"),), ())

        started = json.loads((self.remote / "started.json").read_text())
        self.assertIn("--decider", started["args"])
        request = b"GET /git/workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        answer = self.guest_connects(started, host.ADDRESS, 80, request)
        self.assertEqual(answer[:1], decider.REDIRECT)
        self.assertIn(b"HTTP/1.1 200", answer)
        # The controller fetched origin into the hub, the mirror the hub, and the runner served it.
        self.assertIn(f"{new} refs/heads/main".encode(), answer)
        mirror = objects.parent
        self.assertEqual(run("git", "--git-dir", mirror, "rev-parse", "refs/remotes/origin/main"), new)
        # The runner kept what the bottle was bound with, to bind it again if its bottled restarts.
        self.assertIn('"b"', (self.remote / ".bottle" / "bindings.json").read_text())

    def test_bottle_new_on_a_runner_makes_a_drone_there(self) -> None:
        (self.remote / ".bottle" / "dist" / "bottle-cli").write_text(STATEFUL_CLI)
        # Its base image built there already, from what it's built from now.
        store = self.remote / ".bottle" / "stages"
        store.mkdir(parents=True)
        (store / "base.ext4").write_text("a stage")
        (self.remote / ".bottle" / "images.json").write_text(json.dumps({images.tag("base"): {
            "disk": "base.ext4", "labels": {images.INPUTS_LABEL: features.base_inputs()}}}))
        repo = self.make_repo("app")

        bottle = bottles.create({"features": [], "memory": "512M"}, "drone", [bottles.MountRequest("/workspace", repo)],
                                runner="box")

        self.assertEqual((bottle.runner, bottle.status, bottle.archive), ("box", "ready", "base.ext4"))
        machine = self.remote / "machines" / bottle.machine
        self.assertEqual(machine.read_text().strip(), "running")
        mirror = self.remote / ".bottle" / "git" / f"{mirrors.key(repo / '.git')}.git"
        self.assertEqual(run("git", "--git-dir", mirror, "rev-parse", "refs/heads/main"),
                         run("git", "-C", repo, "rev-parse", "main"))
        execs = (self.remote / "execs").read_text()
        self.assertIn("bottle contract", execs)
        self.assertIn("/mnt/objects/workspace", execs)   # its checkout borrows the mirror's objects, mounted there
        self.assertEqual(bottles.status(["drone"])[0]["state"], "running")

        bottles.stop("drone")
        self.assertEqual(machine.read_text().strip(), "stopped")
        bottles.delete("drone")
        self.assertFalse(machine.exists())
        self.assertEqual(bottles.load(), {})

    def test_a_drone_fetches_again_once_its_controllers_bottled_is_back(self) -> None:
        upstream = self.make_repo("upstream")
        repo = self.tmp / "repo"
        run("git", "clone", "-q", upstream, repo)
        remotes = (Remote("origin", "upstream"), Remote("work", "ns/w", "ns/w"))
        bottles._write({"b": bottles.Bottle("b", "i", "base", 0.0, status="ready", runner="box", mounts=(
            bottles.Mount("/workspace", str(repo), "main", run("git", "-C", repo, "rev-parse", "HEAD"), remotes),))})
        box = runners.get("box")
        box.objects(repos.Repo("repo", repo))
        box.start("bottle-b", "b", bottles.served(bottles.get("b")), ())
        started = json.loads((self.remote / "started.json").read_text())
        fetch = b"GET /git/workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        self.assertIn(b"HTTP/1.1 200", self.guest_connects(started, host.ADDRESS, 80, fetch))

        daemon.stop()
        down = self.guest_connects(started, host.ADDRESS, 80, fetch)
        self.assertIn(b"HTTP/1.1 503", down)
        self.assertIn(b"the controller can't be reached", down)

        daemon.start()
        for _ in range(100):   # the link comes back in the background
            answer = self.guest_connects(started, host.ADDRESS, 80, fetch)
            if b"HTTP/1.1 200" in answer:
                break
            time.sleep(0.2)
        self.assertIn(b"HTTP/1.1 200", answer)
        # And what needs the bottle's side on the controller, a push, finds it, though the controller's bottled
        # is new and the bottle was bound through the one before.
        push = b"GET /git/workspace/work/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        self.assertIn(b"HTTP/1.1 200", self.guest_connects(started, host.ADDRESS, 80, push))

    def test_a_drones_flows_elsewhere_are_decided_there(self) -> None:
        runners.get("box").start("bottle-b", "b", (), ())
        started = json.loads((self.remote / "started.json").read_text())
        answer = self.guest_connects(started, "127.0.0.1", 22, b"SSH-2.0-x\r\n")
        self.assertEqual(answer[:1], decider.DENY)   # the runner's own loopback, never reachable


class DetachTest(DroneCase):
    """A runner detached to be turned off, and attached again."""

    def setUp(self) -> None:
        super().setUp()
        (self.remote / ".bottle" / "dist" / "bottle-cli").write_text(STATEFUL_CLI)
        self.machines = self.remote / "machines"
        self.machines.mkdir()
        bottles._write({name: bottles.Bottle(name, name, "base", 0.0, status="ready", runner="box")
                        for name in ("a", "b")})
        self.a = self.machines / bottles.get("a").machine
        self.a.write_text("running\n")
        (self.machines / bottles.get("b").machine).write_text("stopped\n")

    def off(self):
        """ssh as it is while the runner is off."""
        off = Path(os.environ["BOTTLE_SSH"]).with_name("off-ssh")
        off.write_text(OFF_SSH)
        off.chmod(0o755)
        return mock.patch.dict(os.environ, {"BOTTLE_SSH": str(off)})

    def turn_off(self) -> None:
        patch = self.off()
        patch.start()
        self.addCleanup(patch.stop)

    def test_detaching_stops_its_bottles_and_asks_it_nothing_more(self) -> None:
        asked = []
        self.assertEqual(runners.detach("box", ask=lambda running: asked.append(running) or True), ["a"])

        self.assertEqual(asked, [["a"]])

        self.assertEqual(self.a.read_text().strip(), "stopped")
        self.assertTrue(runners.is_detached("box"))
        self.turn_off()
        self.assertEqual({row["name"]: row["state"] for row in bottles.status()}, {"a": "detached", "b": "detached"})
        self.assertEqual(runners.status(["box"])[0]["detached"], True)
        self.assertEqual(bottles.stop_all(), ([], {}))

    def test_told_no_it_stops_nothing_and_stays_attached(self) -> None:
        for ask in (lambda running: False, None):
            with self.assertRaisesRegex(BottleError, "box is still attached, and a still running"):
                runners.detach("box", ask=ask)
            self.assertEqual(self.a.read_text().strip(), "running")
            self.assertFalse(runners.is_detached("box"))

    def test_with_nothing_running_it_asks_nothing(self) -> None:
        self.a.write_text("stopped\n")
        self.assertEqual(runners.detach("box"), [])
        self.assertTrue(runners.is_detached("box"))

    def test_a_machine_no_bottle_stops_keeps_it_attached(self) -> None:
        (self.machines / "builder-1").write_text("running\n")

        with self.assertRaisesRegex(BottleError, "still runs builder-1"):
            runners.detach("box", ask=lambda running: True)
        self.assertFalse(runners.is_detached("box"))

    def test_one_that_cant_be_reached_is_detached_only_offline(self) -> None:
        self.turn_off()

        with self.assertRaisesRegex(BottleError, "unreachable.*--offline"):
            runners.detach("box", ask=lambda running: True)
        self.assertFalse(runners.is_detached("box"))
        self.assertEqual(runners.detach("box", offline=True), [])
        self.assertTrue(runners.is_detached("box"))

    def test_using_it_again_attaches_it_once_its_back(self) -> None:
        runners.detach("box", ask=lambda running: True)
        with self.off():
            with self.assertRaisesRegex(BottleError, "box is detached, and doesn't answer yet.*turn it on"):
                bottles.ensure_running("a")
            self.assertTrue(runners.is_detached("box"))

        self.assertEqual(runners.get("box").state(self.a.name), "stopped")
        self.assertFalse(runners.is_detached("box"))

    def test_attaching_names_the_bottles_detaching_stopped(self) -> None:
        runners.detach("box", ask=lambda running: True)
        self.assertEqual(runners.attach("box"), ["a"])
        self.assertFalse(runners.is_detached("box"))
