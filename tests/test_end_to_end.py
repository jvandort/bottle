"""A real bottle, end to end: bin/bottle on smolvm, with bottled deciding its connections.

Runs only where smolvm is: a Mac with it on PATH, or $BOTTLE_SMOLVM naming
one. Makes one bottle with `tools` from a clone of this repo, and deletes it
after. Its BOTTLE_HOME is $BOTTLE_E2E_HOME (default /tmp/bottle-e2e), kept
between runs so the images it builds are reused: the first run builds them,
which takes minutes. Its bottled listens on its own decider port, apart from
the user's.

Needs the network: the bottle reaches example.com.
"""

import json
import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from tests.support import requires_smolvm

REPO = Path(__file__).resolve().parent.parent
BOTTLE = REPO / "bin" / "bottle"
NAME = "e2e"


@requires_smolvm
class EndToEndTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        home = Path(os.environ.get("BOTTLE_E2E_HOME") or "/tmp/bottle-e2e")
        home.mkdir(parents=True, exist_ok=True)
        cls.env = {**os.environ, "BOTTLE_HOME": str(home), "BOTTLE_DECIDER_PORT": "47330"}
        cls.log = home / "logs" / "bottled.log"
        cls.bottle("delete", NAME, check=False)
        # Whatever bottled ran here before: this run's code answers the decider.
        cls.bottle("daemon", "stop", check=False)
        time.sleep(1)
        cls.work = Path(tempfile.mkdtemp(prefix="bottle-e2e-"))
        cls.addClassCleanup(shutil.rmtree, cls.work, True)
        cls.addClassCleanup(cls.bottle, "daemon", "stop", check=False)
        cls.repo = cls.work / "repo"
        subprocess.run(["git", "clone", "-q", str(REPO), str(cls.repo)], check=True)
        spec = cls.work / "spec.json"
        spec.write_text(json.dumps({"features": ["tools"], "memory": "8G"}))
        cls.bottle("new", "--spec", str(spec), "--name", NAME, "--git-repo", str(cls.repo),
                   "--git-remote", "origin:fetch=upstream", "--git-remote", "agent:fetch=ns/e2e-out,push=ns/e2e-out")
        cls.addClassCleanup(cls.bottle, "delete", NAME, check=False)

    @classmethod
    def bottle(cls, *args: str, input: bytes | None = None, check: bool = True) -> subprocess.CompletedProcess:
        return subprocess.run([str(BOTTLE), *args], input=input, capture_output=True, env=cls.env, check=check)

    def in_bottle(self, *argv: str, input: bytes | None = None) -> subprocess.CompletedProcess:
        return self.bottle("exec", NAME, "--", *argv, input=input, check=False)

    def out(self, *argv: str) -> str:
        result = self.in_bottle(*argv)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        return result.stdout.decode().strip()

    def https_status(self) -> str:
        return self.out("curl", "-sS", "-o", "/dev/null", "-w", "%{http_code}", "https://example.com")

    def test_its_running(self) -> None:
        [row] = json.loads(self.bottle("status", "--json", NAME).stdout)
        self.assertEqual(row["state"], "running")

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

    def test_the_network_after_a_restart_and_after_bottled_restarts(self) -> None:
        self.bottle("stop", NAME)
        self.assertEqual(self.https_status(), "200")  # exec starts it again, with a new token
        self.bottle("daemon", "stop")
        time.sleep(1)
        self.assertEqual(self.https_status(), "200")  # the next bottled honours the same token
        self.assertIn("e2e", self.log.read_text())


if __name__ == "__main__":
    unittest.main()
