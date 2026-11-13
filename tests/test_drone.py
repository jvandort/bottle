"""A drone, end to end, with ssh and the runner's bottle-cli faked: bottled and this repo's bottle-cli here.

The runner is this machine, in a home of its own, reached through a fake ssh
that runs each command there and makes a forward (-R) a symlink to the
socket here. bottle-cli there records how a machine was started; the test is
then bottle-cli there, giving a socket its machine's routes name a connection
its guest opened: forwarded here, by the process bottle-cli here keeps for
the machine, to bottled.
"""

import json
import os
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from pathlib import Path
from unittest import mock

from bottle import bottles, daemon, features, host, images, prereqs, repos, runners, services
from bottle.host import Remote, Served
from bottle.errors import BottleError
from tests.support import GitTestCase, built_cli, register_runner, run, thread_dir, write_bottles
from tests.test_services import header

FAKE_SSH = r"""#!/usr/bin/env bash
forwards=()
while [ "$1" != "--" ]; do [ "$1" = -R ] && forwards+=("$2"); shift; done
shift; destination=$1; shift
home="$FAKE_REMOTES/$destination"; mkdir -p "$home"
if [ ${#forwards[@]} -gt 0 ]; then
  for f in "${forwards[@]}"; do ln -sfn "${f#*:}" "${f%%:*}"; done
  exec sleep 3600
fi
cd "$home" && exec env -u BOTTLE_HOME HOME="$home" bash -c "$*"
"""

# bottle-cli there: a machine started is recorded, with what it was given; every machine is running.
FAKE_CLI = r"""#!/usr/bin/env bash
case "$1 $2" in
  "machine start") printf '{"args": "%s"}' "$*" > "$HOME/started.json" ;;
  "machine create"|"machine configure") cat > "$HOME/network.json" ;;
  "info --json") printf '{"memory_mib": 4096, "cpus": 32, "home": "%s/.bottle"}' "$HOME" ;;
  *) echo '{"state": "running"}' ;;
esac
"""


# bottle-cli there with machines: made, started, stopped, deleted and asked about, each a file; exec runs nothing.
STATEFUL_CLI = r"""#!/usr/bin/env bash
# Its images: as the test wrote them. Its host, and its CAs.
[ "$1" = images ] && exec cat "$HOME/images.listed"
[ "$1" = info ] && { printf '{"memory_mib": 4096, "cpus": 32, "home": "%s/.bottle"}' "$HOME"; exit 0; }
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
        # The same build as what bottle-cli here would copy there: not copied.
        (cli.parent / prereqs.STAMP).write_text("these sources")
        arch = subprocess.run(["uname", "-m"], capture_output=True, text=True).stdout.strip()
        dist = short / "dists" / f"dist-{arch}-unknown-linux-musl"
        dist.mkdir(parents=True)
        (dist / prereqs.STAMP).write_text("these sources")
        fake = short / "ssh"
        fake.write_text(FAKE_SSH)
        fake.chmod(0o755)
        self.controller = short / "controller"
        env = mock.patch.dict(os.environ, {
            "BOTTLE_HOME": str(self.controller), "BOTTLE_SSH": str(fake), "FAKE_REMOTES": str(short / "r"),
            "BOTTLE_CLI": str(built_cli()), "BOTTLE_RUNNER_DISTS": str(short / "dists"),
        })
        env.start()
        self.addCleanup(env.stop)
        self.addCleanup(self.stop_both)
        for name in ("ensure_dist", "ensure_cli"):
            patcher = mock.patch.object(prereqs, name)
            patcher.start()
            self.addCleanup(patcher.stop)
        register_runner("box", "box")

    def stop_both(self) -> None:
        daemon.stop()
        # And the forwards bottle-cli keeps for machines there.
        for pid in self.controller.glob("krun/runners/*/*/forward.pid"):
            try:
                os.killpg(int(pid.read_text()), signal.SIGTERM)
            except (ProcessLookupError, ValueError):
                pass

    def made(self, machine: str) -> None:
        """`machine` made on box, routed as a bottle's is (services.network)."""
        runners.get("box").create(machine, Path("/stages/base.ext4"), [], {}, 512,
                                  network=services.network([], []))

    def guest_connects(self, machine: str, address: str, port: int, first: bytes, said: str | None = None) -> bytes:
        """A connection from `machine`'s guest, as bottle-cli there gives it to the socket its routes name, forwarded
        to bottled here: the machine it's from as the PROXY header says, `machine` unless `said`."""
        forwards = json.loads((self.controller / "krun" / "runners" / "box" / machine / "forwards.json").read_text())
        there = Path(forwards[0][0])
        for _ in range(100):   # its forward, made in the background as it started
            if there.exists():
                break
            time.sleep(0.1)
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(10)
            s.connect(str(there))
            s.sendall(header(address, port, machine=said or machine) + first)
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
        write_bottles({"b": bottles.Bottle("b", "i", "base", 0.0, status="ready", runner="box", mounts=(
            bottles.Mount("/workspace", str(repo), "main", run("git", "-C", repo, "rev-parse", "HEAD"), remotes),))})
        box = runners.get("box")

        # What its machine mounts: a bare mirror there, pushed into from here.
        objects = box.objects(repos.Repo("repo", repo))
        mirror = self.remote / ".bottle" / "git" / f"{runners.mirror_key(repo / '.git')}.git"
        self.assertEqual(objects, mirror / "objects")
        self.assertEqual(run("git", "--git-dir", mirror, "rev-parse", "refs/heads/main"),
                         run("git", "-C", repo, "rev-parse", "main"))
        new = self.commit(upstream, "since the controller last fetched")
        machine = bottles.get("b").machine
        self.made(machine)
        box.start(machine, "b", (Served(repo / ".git", remotes, "workspace"),), ())

        started = json.loads((self.remote / "started.json").read_text())
        self.assertEqual(started["args"], f"machine start --name {machine}")   # nothing else: its policy is its own
        request = b"GET /git/workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        answer = self.guest_connects(machine, host.ADDRESS, 80, request)
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

        bottle = bottles.create(thread_dir(), {"features": [], "memory": "512M"}, "drone", [bottles.MountRequest("/workspace", repo)],
                                runner="box")

        self.assertEqual((bottle.runner, bottle.status, bottle.archive), ("box", "ready", "base.ext4"))
        machine = self.remote / "machines" / bottle.machine
        self.assertEqual(machine.read_text().strip(), "running")
        # Its routes, to a socket of its own there, forwarded here.
        network = json.loads((self.remote / "machines" / f"{bottle.machine}.network").read_text())
        theres = {r["socket"] for r in network["routes"]}
        self.assertEqual(len(theres), 2)   # bottle.host's, ssh's: a socket there for each socket here
        self.assertTrue(all(t.startswith(str(self.remote / ".bottle" / "routes")) for t in theres), theres)
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
        write_bottles({"b": bottles.Bottle("b", "i", "base", 0.0, status="ready", runner="box", mounts=(
            bottles.Mount("/workspace", str(repo), "main", run("git", "-C", repo, "rev-parse", "HEAD"), remotes),))})
        box = runners.get("box")
        box.objects(repos.Repo("repo", repo))
        machine = bottles.get("b").machine   # as bottled, restarted, binds it again
        self.made(machine)
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
        self.made("bottle-b")
        runners.get("box").start("bottle-b", "b", (), ())
        fetch = b"GET /git/workspace/origin/info/refs HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        self.assertEqual(self.guest_connects("bottle-b", host.ADDRESS, 80, fetch, said="stranger"), b"")


class DetachTest(DroneCase):
    """A runner detached to be turned off, and attached again."""

    def setUp(self) -> None:
        super().setUp()
        (self.remote / ".bottle" / "dist" / "bottle-cli").write_text(STATEFUL_CLI)
        self.machines = self.remote / "machines"
        self.machines.mkdir()
        write_bottles({name: bottles.Bottle(name, name, "base", 0.0, status="ready", runner="box")
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

    def bottle_cli(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run([os.environ["BOTTLE_CLI"], *args], capture_output=True, text=True,
                              stdin=subprocess.DEVNULL)

    def test_with_its_bottles_running_and_no_one_to_ask_it_stops_nothing_and_stays_attached(self) -> None:
        detached = self.bottle_cli("runner", "detach", "box")
        self.assertNotEqual(detached.returncode, 0)
        self.assertIn("box is still attached, and box/" + self.a.name + " still running", detached.stderr)
        self.assertEqual(self.a.read_text().strip(), "running")
        self.assertFalse(runners.is_detached("box"))

    def test_detached_its_bottles_are_detached_and_it_isnt_asked(self) -> None:
        self.a.write_text("stopped\n")
        self.assertEqual(self.bottle_cli("runner", "detach", "box").returncode, 0)
        self.assertTrue(runners.is_detached("box"))
        self.turn_off()
        self.assertEqual({row["name"]: row["state"] for row in bottles.status()}, {"a": "detached", "b": "detached"})

    def test_one_that_cant_be_reached_is_detached_only_offline(self) -> None:
        self.turn_off()
        self.assertIn("--offline", self.bottle_cli("runner", "detach", "box").stderr)
        self.assertFalse(runners.is_detached("box"))
        self.assertEqual(self.bottle_cli("runner", "detach", "box", "--offline").returncode, 0)
        self.assertTrue(runners.is_detached("box"))

    def test_using_it_again_attaches_it_once_its_back(self) -> None:
        self.bottle_cli("runner", "detach", "box", "--offline")
        with self.off():
            with self.assertRaisesRegex(BottleError, "box is detached, and doesn't answer yet.*turn it on"):
                bottles.ensure_running("a")
            self.assertTrue(runners.is_detached("box"))

        self.assertEqual(runners.get("box").state(self.a.name), "running")
        self.assertFalse(runners.is_detached("box"))

