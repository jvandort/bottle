"""A drone: a thread's bottle on a runner, its machine answered by folio serve here, through the forward folio serve
keeps to the runner (tests/fake_ssh makes one a symlink there to the socket here). The test is the runner's
bottle-cli: it gives the socket the machine's routes name a connection its guest opened, PROXY header and all."""

import socket
import time

from tests.bottle.support import header
from tests.support import git
from tests.test_machines import RunnerTestCase

FETCH = b"GET /git/workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
PUSH = b"GET /git/workspace/agent/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n"


class DroneCase(RunnerTestCase):
    def setUp(self) -> None:
        super().setUp()
        # The repo is a clone of its upstream, which gets commits the repo never fetched; named as the repo is, so
        # the repo's name, which is its origin's, stays the same.
        self.upstream = self.make_repo("upstream/project")
        git("-C", str(self.repo), "remote", "add", "origin", str(self.upstream))
        git("-C", str(self.repo), "fetch", "-q", "origin")
        self.ok("new", "t", "--runner", "box")
        self.machine = f"{self.key}-t"

    def guest_connects(self, first: bytes, said: str | None = None) -> bytes:
        """A connection from the machine's guest to bottle.host, as bottle-cli there gives it to the socket its
        routes name, from the machine its PROXY header says (its own, unless `said`). One closed unanswered may be
        reset, if it hadn't read all that was sent."""
        there = self.there()["machines"][self.machine]["network"]["routes"][0]["socket"]
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:   # its runner's forward, made in the background as it was bound
            try:
                with socket.socket(socket.AF_UNIX) as s:
                    s.settimeout(10)
                    s.connect(there)
                    s.sendall(header("192.0.2.1", 80, "bottle.host", machine=said or self.machine) + first)
                    answer = b""
                    try:
                        while chunk := s.recv(65536):
                            answer += chunk
                    except ConnectionResetError:
                        pass
                    return answer
            except (FileNotFoundError, ConnectionRefusedError):
                time.sleep(0.1)
        raise ConnectionRefusedError(f"nothing at {there}")


class DroneTest(DroneCase):
    def test_its_fetch_reaches_the_hub_here_through_its_runners_forward(self) -> None:
        new = self.commit(self.upstream, "since the hub last fetched")
        answer = self.guest_connects(FETCH)
        self.assertIn(b"HTTP/1.1 200", answer)
        # The hub fetched origin, and served it, through the runner's forward.
        self.assertIn(f"{new} refs/heads/main".encode(), answer)

    def test_it_fetches_and_pushes_again_once_folio_serve_is_back(self) -> None:
        self.assertIn(b"HTTP/1.1 200", self.guest_connects(FETCH))
        self.ok("serve", "stop")
        # Gone (Python's folio returned as it wound up; folio's waits).
        deadline = time.monotonic() + 5
        while (self.home / "serve.sock").exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        there = self.there()["machines"][self.machine]["network"]["routes"][0]["socket"]
        with socket.socket(socket.AF_UNIX) as s, self.assertRaises(OSError):   # its forward's gone with it
            s.connect(there)
        self.ok("serve", "start")   # binding again the bottles whose machines run, wherever they are
        self.assertIn(b"HTTP/1.1 200", self.guest_connects(FETCH))
        # A push, which needs the bottle's side here, finds it, though folio serve is new.
        self.assertIn(b"HTTP/1.1 200", self.guest_connects(PUSH))

    def test_a_machine_no_bottle_is_gets_nothing(self) -> None:
        self.assertEqual(self.guest_connects(FETCH, said="stranger"), b"")


class DetachTest(DroneCase):
    """A runner detached to be turned off, and attached again."""

    def test_with_its_bottles_running_and_no_one_to_ask_it_stops_nothing_and_stays_attached(self) -> None:
        detached = self.folio("runner", "detach", "box")
        self.assertNotEqual(detached.returncode, 0)
        self.assertIn(f"box is still attached, and {self.machine} still running", detached.stderr)
        self.assertEqual(self.there()["machines"][self.machine]["state"], "running")
        self.assertNotIn("(detached)", self.ok("runner", "ls"))

    def test_detached_its_bottles_are_detached_and_it_isnt_asked(self) -> None:
        self.set_there("t", state="stopped")
        self.ok("runner", "detach", "box")
        self.off.write_text("")
        rows = {line.split()[0]: line.split()[1] for line in self.ok("list").splitlines()[1:]}
        self.assertEqual(rows, {"t": "detached"})

    def test_one_that_cant_be_reached_is_detached_only_offline(self) -> None:
        self.off.write_text("")
        self.assertIn("--offline", self.folio("runner", "detach", "box").stderr)
        self.assertNotIn("(detached)", self.ok("runner", "ls"))
        self.ok("runner", "detach", "box", "--offline")
        self.assertIn("(detached)", self.ok("runner", "ls"))

    def test_using_it_again_attaches_it_once_its_back(self) -> None:
        self.off.write_text("")
        self.ok("runner", "detach", "box", "--offline")
        refused = self.folio("exec", "t", "--", "true")
        self.assertEqual(refused.returncode, 1)
        self.assertRegex(refused.stderr, "box is detached, and doesn't answer yet.*turn it on")
        self.assertIn("(detached)", self.ok("runner", "ls"))
        self.off.unlink()
        result = self.folio("exec", "t", "--", "true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("(detached)", self.ok("runner", "ls"))
