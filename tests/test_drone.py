"""A drone, end to end, with only ssh and bottle-cli faked: the controller's bottled, the link, a runner's bottled.

The runner is this machine, in a home of its own, reached through a fake ssh
that runs each command there and makes a forward (-R) a symlink to the
socket here. bottle-cli there records how a machine was started; the test is
then bottle-cli, giving the runner's bottled a connection its guest opened, as
the machine's policy routes it.
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

from bottle import bottles, daemon, features, host, images, prereqs, repos, runners
from bottle.host import Remote, Served
from bottle.errors import BottleError
from tests.support import GitTestCase, run
from tests.test_services import header

FAKE_SSH = r"""#!/usr/bin/env bash
forward=
while [ "$1" != "--" ]; do [ "$1" = -R ] && forward=$2; shift; done
shift; destination=$1; shift
mkdir -p "$FAKE_REMOTES/$destination"
if [ -n "$forward" ]; then ln -sfn "${forward#*:}" "${forward%%:*}"; exec sleep 3600; fi
exec env -u BOTTLE_HOME HOME="$FAKE_REMOTES/$destination" bash -c "$1"
"""

# bottle-cli there: a machine started is recorded, with what it was given.
FAKE_CLI = r"""#!/usr/bin/env bash
if [ "$1 $2" = "machine start" ]; then
  printf '{"args": "%s"}' "$*" > "$HOME/started.json"
fi
echo '{"state": "running"}'
"""


# bottle-cli there with machines: made, started, stopped, deleted and asked about, each a file; exec runs nothing.
STATEFUL_CLI = r"""#!/usr/bin/env bash
# Its images: as the test wrote them. Its host, and its CAs.
[ "$1" = images ] && exec cat "$HOME/images.listed"
[ "$1" = info ] && { echo '{"memory_mib": 4096, "cpus": 32}'; exit 0; }
[ "$1 $2" = "ca certificate" ] && { echo "a CA of $3's"; exit 0; }
op="$1 $2"; shift 2
while [ $# -gt 0 ]; do [ "$1" = --name ] && name=$2; [ "$1" = -- ] && break; shift; done
machines="$HOME/machines"; mkdir -p "$machines"
case "$op" in
  "machine create") echo created > "$machines/$name"; cat > "$machines/$name.network" ;;
  "machine start")
    echo running > "$machines/$name"
    printf '{"args": "%s"}' "$*" > "$HOME/started.json" ;;
  "machine stop") echo stopped > "$machines/$name" ;;
  "machine delete") rm -f "$machines/$name" ;;
  "machine status")
    if [ -f "$machines/$name" ]; then printf '{"state": "%s"}' "$(cat "$machines/$name")"
    else echo "bottle-cli: no machine $name" >&2; exit 1; fi ;;
  "machine exec") cat > /dev/null; echo "$*" >> "$HOME/execs" ;;
  "machine ls")
    printf '['; sep=
    for m in "$machines"/*; do
      case "$m" in *.network) continue ;; esac
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
        # Built from these sources, as far as the controller can tell (see ssh.SshTransport.ensure_dist): not copied.
        (cli.parent / prereqs.STAMP).write_text(prereqs.source_hash())
        fake = short / "ssh"
        fake.write_text(FAKE_SSH)
        fake.chmod(0o755)
        env = mock.patch.dict(os.environ, {
            "BOTTLE_HOME": str(short / "controller"), "BOTTLE_SSH": str(fake), "FAKE_REMOTES": str(short / "r"),
        })
        env.start()
        self.addCleanup(env.stop)
        self.addCleanup(self.stop_both)
        runners.register("box", "box")

    def stop_both(self) -> None:
        daemon.stop()  # the controller's, and with it its forwards

    def guest_connects(self, machine: str, address: str, port: int, first: bytes) -> bytes:
        """A connection from `machine`'s guest, as bottle-cli there gives it to the socket its routes name: one ssh
        forwards to bottled on the controller."""
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(10)
            s.connect(str(self.remote / ".bottle" / "services.sock"))
            s.sendall(header(address, port, machine=machine) + first)
            answer = b""
            while chunk := s.recv(65536):
                answer += chunk
        return answer



class DroneTest(DroneCase):
    def test_a_drones_fetch_reaches_the_hub_on_the_controller_through_its_runners_forward(self) -> None:
        upstream = self.make_repo("upstream")
        repo = self.tmp / "repo"
        run("git", "clone", "-q", upstream, repo)
        remotes = (Remote("origin", "upstream"),)
        bottles._write({"b": bottles.Bottle("b", "i", "base", 0.0, status="ready", runner="box", mounts=(
            bottles.Mount("/workspace", str(repo), "main", run("git", "-C", repo, "rev-parse", "HEAD"), remotes),))})
        box = runners.get("box")

        # What its machine mounts: a bare mirror there, pushed into from here.
        objects = box.objects(repos.Repo("repo", repo))
        mirror = self.remote / ".bottle" / "git" / f"{runners.mirror_key(repo / '.git')}.git"
        self.assertEqual(objects, mirror / "objects")
        self.assertEqual(run("git", "--git-dir", mirror, "rev-parse", "refs/heads/main"),
                         run("git", "-C", repo, "rev-parse", "main"))
        new = self.commit(upstream, "since the controller last fetched")
        box.start("bottle-b", "b", (Served(repo / ".git", remotes, "workspace"),), ())

        started = json.loads((self.remote / "started.json").read_text())
        self.assertEqual(started["args"], "machine start --name bottle-b")   # nothing else: its policy is its own
        request = b"GET /git/workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        answer = self.guest_connects("bottle-b", host.ADDRESS, 80, request)
        self.assertIn(b"HTTP/1.1 200", answer)
        # The controller fetched origin into the hub, and served it, through the forward.
        self.assertIn(f"{new} refs/heads/main".encode(), answer)

    def test_bottle_new_on_a_runner_makes_a_drone_there(self) -> None:
        (self.remote / ".bottle" / "dist" / "bottle-cli").write_text(STATEFUL_CLI)
        # Its base image built there already, from what it's built from now.
        store = self.remote / ".bottle" / "stages"
        store.mkdir(parents=True)
        (store / "base.ext4").write_text("a stage")
        (self.remote / "images.listed").write_text(json.dumps([{
            "tag": images.tag("base"), "name": "base", "disks": [str(store / "base.ext4")],
            "labels": {images.INPUTS_LABEL: features.base_inputs()}}]))
        repo = self.make_repo("app")

        bottle = bottles.create({"features": [], "memory": "512M"}, "drone", [bottles.MountRequest("/workspace", repo)],
                                runner="box")

        self.assertEqual((bottle.runner, bottle.status, bottle.archive), ("box", "ready", "base.ext4"))
        machine = self.remote / "machines" / bottle.machine
        self.assertEqual(machine.read_text().strip(), "running")
        # Its routes, to the socket there that's forwarded here.
        network = json.loads((self.remote / "machines" / f"{bottle.machine}.network").read_text())
        self.assertEqual({r["socket"] for r in network["routes"]}, {str(self.remote / ".bottle" / "services.sock")})
        mirror = self.remote / ".bottle" / "git" / f"{runners.mirror_key(repo / '.git')}.git"
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
        machine = bottles.get("b").machine   # as bottled, restarted, binds it again
        box.start(machine, "b", bottles.served(bottles.get("b")), ())
        fetch = b"GET /git/workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        self.assertIn(b"HTTP/1.1 200", self.guest_connects(machine, host.ADDRESS, 80, fetch))

        daemon.stop()
        with self.assertRaises(OSError):   # the forward's gone with it: refused, as bottle-cli would be
            self.guest_connects(machine, host.ADDRESS, 80, fetch)

        daemon.start()
        answer = b""
        for _ in range(100):   # the forward comes back in the background
            try:
                answer = self.guest_connects(machine, host.ADDRESS, 80, fetch)
            except OSError:
                answer = b""
            if b"HTTP/1.1 200" in answer:
                break
            time.sleep(0.2)
        self.assertIn(b"HTTP/1.1 200", answer)
        # And what needs the bottle's side on the controller, a push, finds it, though the controller's bottled
        # is new and the bottle was bound through the one before.
        push = b"GET /git/workspace/work/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        self.assertIn(b"HTTP/1.1 200", self.guest_connects(machine, host.ADDRESS, 80, push))

    def test_what_a_machine_no_bottle_is_routes_gets_nothing_there(self) -> None:
        runners.get("box").start("bottle-b", "b", (), ())
        fetch = b"GET /git/workspace/origin/info/refs HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        self.assertEqual(self.guest_connects("stranger", host.ADDRESS, 80, fetch), b"")


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
