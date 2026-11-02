"""A drone, end to end, with only ssh and smolvm faked: the controller's bottled, the link, a runner's bottled.

The runner is this machine, in a home of its own, reached through a fake ssh
that runs each command there and makes a forward (-R) a symlink to the
socket here. Each bottled's decider listens on a port of its own choosing
(BOTTLE_DECIDER_PORT=0), so neither meets a real one. smolvm there records
how a machine was started; the test is then its guest, opening a connection
through the runner's decider as smolvm would.
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

from bottle import bottles, daemon, decider, host, images, mirrors, repos, runners
from bottle.host import Remote, Served
from tests.support import GitTestCase, run

FAKE_SSH = r"""#!/usr/bin/env bash
forward=
while [ "$1" != "--" ]; do [ "$1" = -R ] && forward=$2; shift; done
shift; destination=$1; shift
mkdir -p "$FAKE_REMOTES/$destination"
if [ -n "$forward" ]; then ln -sfn "${forward#*:}" "${forward%%:*}"; exec sleep 3600; fi
exec env -u BOTTLE_HOME HOME="$FAKE_REMOTES/$destination" bash -c "$1"
"""

# smolvm there: a machine started is recorded, with the decider and token it was given.
FAKE_SMOLVM = r"""#!/usr/bin/env bash
if [ "$1 $2 $3 $4" = "--error-format json machine start" ]; then
  printf '{"args": "%s", "token": "%s"}' "$*" "$SMOLVM_INTERCEPTOR_TOKEN" > "$HOME/started.json"
fi
echo '{"state": "running"}'
"""


# smolvm there with machines: made, started, stopped, deleted and asked about, each a file; exec runs nothing.
STATEFUL_SMOLVM = r"""#!/usr/bin/env bash
shift 2  # --error-format json
op="$1 $2"; shift 2
while [ $# -gt 0 ]; do [ "$1" = --name ] && name=$2; [ "$1" = -- ] && break; shift; done
machines="$HOME/machines"; mkdir -p "$machines"
case "$op" in
  "machine create") echo created > "$machines/$name" ;;
  "machine start")
    echo running > "$machines/$name"
    printf '{"args": "%s", "token": "%s"}' "$*" "$SMOLVM_INTERCEPTOR_TOKEN" > "$HOME/started.json" ;;
  "machine stop") echo stopped > "$machines/$name" ;;
  "machine delete") rm -f "$machines/$name" ;;
  "machine status")
    if [ -f "$machines/$name" ]; then printf '{"state": "%s"}' "$(cat "$machines/$name")"
    else echo '{"error": {"code": "NOT_FOUND", "message": "machine not found"}}' >&2; exit 1; fi ;;
  "machine exec") cat > /dev/null; echo "$*" >> "$HOME/execs" ;;
esac
"""


class DroneTest(GitTestCase):
    def setUp(self) -> None:
        super().setUp()
        short = Path(tempfile.mkdtemp(dir="/tmp"))   # macOS caps Unix socket paths at 104 bytes
        self.addCleanup(shutil.rmtree, short, ignore_errors=True)
        self.remote = short / "r" / "box"
        smolvm = self.remote / ".bottle" / "smolvm" / "current" / "smolvm"
        smolvm.parent.mkdir(parents=True)
        smolvm.write_text(FAKE_SMOLVM)
        smolvm.chmod(0o755)
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
        """A connection from the machine's guest, as smolvm hands it to the decider it was given."""
        decider_address = started["args"].split("--egress-interceptor ")[1].split()[0]
        ip, _, decider_port = decider_address.rpartition(":")
        with socket.create_connection((ip, int(decider_port)), timeout=10) as s:
            s.sendall(decider.MAGIC + bytes.fromhex(started["token"]) + b"m" * 16 + bytes(16) + bytes([4])
                      + port.to_bytes(2, "big") + socket.inet_aton(address) + len(first).to_bytes(2, "big") + first)
            answer = b""
            while chunk := s.recv(65536):
                answer += chunk
        return answer

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
        self.assertIn("--mediated-egress", started["args"])
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
        (self.remote / ".bottle" / "smolvm" / "current" / "smolvm").write_text(STATEFUL_SMOLVM)
        # Its base image built there already, from what it's built from now.
        store = self.remote / ".bottle" / "images"
        store.mkdir(parents=True)
        (store / "base.tar").write_text("an archive")
        (store / "index.json").write_text(json.dumps({images.tag("base"): {
            "archive": "base.tar", "labels": {images.INPUTS_LABEL: images.inputs_hash("base")}}}))
        repo = self.make_repo("app")

        bottle = bottles.create({"features": [], "memory": "512M"}, "drone", [bottles.MountRequest("/workspace", repo)],
                                runner="box")

        self.assertEqual((bottle.runner, bottle.status, bottle.archive), ("box", "ready", "base.tar"))
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
