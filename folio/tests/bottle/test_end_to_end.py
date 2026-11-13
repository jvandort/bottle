"""A real bottle, end to end: made by bottles.py, as folio makes one, on bottle-cli's machines, with folio serve
answering what its machine routes to it; run in through bin/bottle, bottle-cli.

Runs only where bottle-cli can run VMs: a Mac, with bottle-cli built or $BOTTLE_CLI naming
one. Makes one bottle with `tools` from a clone of this repo, and deletes it
after. Its BOTTLE_HOME is $BOTTLE_E2E_HOME (default /tmp/bottle-e2e), kept
between runs so the images it builds are reused: the first run builds them,
which takes minutes. Its folio home, and so its folio serve, is its own, in it.

Needs the network: the bottle reaches example.com.
"""

import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from folio.bottle import bottles, daemon
from folio.bottle.errors import BottleError
from tests.bottle.support import requires_krun, thread_dir

REPO = Path(__file__).resolve().parents[3]
BOTTLE = REPO / "bin" / "bottle"
NAME = "e2e"


@requires_krun
class EndToEndTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        home = Path(os.environ.get("BOTTLE_E2E_HOME") or "/tmp/bottle-e2e")
        home.mkdir(parents=True, exist_ok=True)
        env = mock.patch.dict(os.environ, {"BOTTLE_HOME": str(home), "FOLIO_HOME": str(home / "folio")})
        env.start()
        cls.addClassCleanup(env.stop)
        cls.log = home / "folio" / "logs" / "serve.log"
        cls.delete()
        # Whatever folio serve ran here before: this run's code answers.
        daemon.stop()
        cls.work = Path(tempfile.mkdtemp(prefix="bottle-e2e-"))
        cls.addClassCleanup(shutil.rmtree, cls.work, True)
        cls.addClassCleanup(daemon.stop)
        cls.repo = cls.work / "repo"
        subprocess.run(["git", "clone", "-q", str(REPO), str(cls.repo)], check=True)
        bottles.create(thread_dir(), {"features": ["tools"], "memory": "8G"}, NAME, [bottles.MountRequest(
            "/workspace", cls.repo, remotes=["origin:fetch=upstream", "agent:fetch=ns/e2e-out,push=ns/e2e-out"])])
        cls.addClassCleanup(cls.delete)
        cls.machine = bottles.get(NAME).machine

    @classmethod
    def delete(cls) -> None:
        try:
            bottles.delete(NAME)
        except BottleError:
            pass

    @classmethod
    def bottle(cls, *args: str, input: bytes | None = None, check: bool = True) -> subprocess.CompletedProcess:
        return subprocess.run([str(BOTTLE), *args], input=input, capture_output=True, check=check)

    def in_bottle(self, *argv: str, input: bytes | None = None) -> subprocess.CompletedProcess:
        return self.bottle("exec", self.machine, "--", *argv, input=input, check=False)

    def out(self, *argv: str) -> str:
        result = self.in_bottle(*argv)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        return result.stdout.decode().strip()

    def https_status(self) -> str:
        return self.out("curl", "-sS", "-o", "/dev/null", "-w", "%{http_code}", "https://example.com")

    def test_its_running(self) -> None:
        [row] = bottles.status([NAME])
        self.assertEqual(row["state"], "running")
        self.assertEqual(self.bottle("status", self.machine).stdout.decode().strip(), "running")

    def test_cp_in_and_out(self) -> None:
        (self.work / "f").write_text("e2e\n")
        self.bottle("cp", str(self.work / "f"), f"{self.machine}:/tmp/f")
        self.assertEqual(self.out("cat", "/tmp/f"), "e2e")
        self.bottle("cp", f"{self.machine}:/tmp/f", str(self.work / "back"))
        self.assertEqual((self.work / "back").read_text(), "e2e\n")

    def test_the_network_works_with_no_proxy(self) -> None:
        self.assertEqual(self.out("sh", "-c", "env | grep -ci proxy || true"), "0")
        self.assertIn("works as usual", self.out("sh", "-c", "cat ~/BOTTLE.md"))
        self.assertTrue(self.out("getent", "hosts", "example.com"))
        self.assertEqual(self.https_status(), "200")

    def test_a_private_address_is_refused_at_once(self) -> None:
        started = time.monotonic()
        result = self.in_bottle("curl", "-s", "-m", "10", "-o", "/dev/null", "http://192.168.1.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(time.monotonic() - started, 5)

    def test_git_through_bottle_host(self) -> None:
        self.assertIn("bottle.host", self.out("cat", "/etc/hosts"))
        self.out("git", "-C", "/workspace", "fetch", "-q", "origin")
        self.out("sh", "-c", 'cd /workspace && git commit -q --allow-empty -m e2e && git push -q agent HEAD:refs/heads/e2e')
        landed = subprocess.run(["git", "-C", str(self.repo), "for-each-ref", "refs/namespaces/e2e-out/refs/heads/e2e"],
                                capture_output=True, text=True, check=True).stdout
        self.assertTrue(landed, "the push didn't land in its namespace")

    def test_exec_carries_bytes_and_status(self) -> None:
        self.assertEqual(len(self.in_bottle("head", "-c", "50000000", "/dev/urandom").stdout), 50_000_000)
        self.assertEqual(self.in_bottle("wc", "-c", input=bytes(20_000_000)).stdout.strip(), b"20000000")
        self.assertEqual(self.in_bottle("sh", "-c", "exit 7").returncode, 7)

    def test_the_network_after_a_restart_and_after_folio_serve_restarts(self) -> None:
        self.bottle("stop", self.machine)
        self.assertEqual(self.https_status(), "200")  # exec starts it again, with its policy
        daemon.stop()
        bottles.ensure_running(NAME)                   # folio serve, started again, answers it
        self.out("git", "-C", "/workspace", "fetch", "-q", "origin")
        self.assertIn("e2e", self.log.read_text())


if __name__ == "__main__":
    unittest.main()
