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

from bottle import bottles, daemon, decider, host, mirrors, repos, runners
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

    def test_a_drones_flows_elsewhere_are_decided_there(self) -> None:
        runners.get("box").start("bottle-b", "b", (), ())
        started = json.loads((self.remote / "started.json").read_text())
        answer = self.guest_connects(started, "127.0.0.1", 22, b"SSH-2.0-x\r\n")
        self.assertEqual(answer[:1], decider.DENY)   # the runner's own loopback, never reachable
