"""A real thread, end to end: made by folio, as you make one, on bottle-cli's machines, with folio serve answering
what its machine routes to it; run in through `folio exec`, and bin/bottle for its machine.

Runs only where bottle-cli can run VMs, a Mac, and only with FOLIO_VM_TESTS=1: this checkout's bottle-cli, built
first if it's changed (bin/bottle-build), or $BOTTLE_CLI. Makes one thread, `e2e`, from a clone of this repo, its spec `tools` and 8G, and deletes it after. Its
BOTTLE_HOME is $BOTTLE_E2E_HOME (default /tmp/bottle-e2e), kept between runs so the images it builds are reused:
the first run builds them, which takes minutes. Its folio home, and so its folio serve, is its own, in it.

Needs the network: the bottle reaches example.com.
"""

import os
import platform
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from tests.support import FOLIO, real_bottle_cli

REPO = Path(__file__).resolve().parents[2]
BOTTLE = REPO / "bin" / "bottle"
THREAD = "e2e"


class EndToEndTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        bottle_cli = real_bottle_cli()
        home = Path(os.environ.get("BOTTLE_E2E_HOME") or "/tmp/bottle-e2e")
        home.mkdir(parents=True, exist_ok=True)
        cls.env = {**os.environ, "BOTTLE_HOME": str(home), "FOLIO_HOME": str(home / "folio"),
                   "BOTTLE_CLI": str(bottle_cli)}
        cls.env.pop("FOLIO_BOTTLE", None)
        cls.log = home / "folio" / "logs" / "serve.log"
        cls.work = Path(tempfile.mkdtemp(prefix="bottle-e2e-"))
        cls.addClassCleanup(shutil.rmtree, cls.work, True)
        cls.repo = cls.work / "repo"
        subprocess.run(["git", "clone", "-q", str(REPO), str(cls.repo)], check=True)
        # Whatever folio serve ran here before: this run's code answers.
        cls.folio("serve", "stop")
        cls.addClassCleanup(cls.folio, "serve", "stop")
        cls.folio("configure", "--memory", "8G", check=True)
        cls.folio("new", THREAD, check=True)
        cls.addClassCleanup(cls.folio, "delete", THREAD, "--yes")
        # Its bottle's machine: the bottle's name, as its record has it.
        cls.machine = next(p.stem for p in Path(cls.env["FOLIO_HOME"]).glob(f"*/{THREAD}/bottles/*.json"))

    @classmethod
    def folio(cls, *args: str, check: bool = False) -> subprocess.CompletedProcess:
        result = subprocess.run([str(FOLIO), *args], cwd=cls.repo, env=cls.env, capture_output=True)
        if check and result.returncode != 0:
            raise AssertionError(f"folio {' '.join(args)}: {result.stderr.decode()}")
        return result

    @classmethod
    def bottle(cls, *args: str, input: bytes | None = None, check: bool = True) -> subprocess.CompletedProcess:
        return subprocess.run([str(BOTTLE), *args], input=input, capture_output=True, check=check, env=cls.env)

    def in_bottle(self, *argv: str, input: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run([str(FOLIO), "exec", THREAD, "--", *argv], cwd=self.repo, env=self.env, input=input,
                              capture_output=True)

    def out(self, *argv: str) -> str:
        result = self.in_bottle(*argv)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        return result.stdout.decode().strip()

    def https_status(self) -> str:
        return self.out("curl", "-sS", "-o", "/dev/null", "-w", "%{http_code}", "https://example.com")

    def test_its_running(self) -> None:
        rows = {line.split()[0]: line.split()[1] for line in self.folio("list").stdout.decode().splitlines()[1:]}
        self.assertEqual(rows[THREAD], "running")
        self.assertEqual(self.bottle("status", self.machine).stdout.decode().strip(), "running")

    def test_cp_in_and_out(self) -> None:
        (self.work / "f").write_text("e2e\n")
        self.folio("cp", str(self.work / "f"), f"{THREAD}:/tmp/f", check=True)
        self.assertEqual(self.out("cat", "/tmp/f"), "e2e")
        self.folio("cp", f"{THREAD}:/tmp/f", str(self.work / "back"), check=True)
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
        self.out("sh", "-c", "cd /workspace && git commit -q --allow-empty -m e2e && git push -q agent HEAD:refs/heads/e2e")
        landed = subprocess.run(["git", "-C", str(self.repo), "for-each-ref", f"refs/namespaces/{THREAD}-out/refs/heads/e2e"],
                                capture_output=True, text=True, check=True).stdout
        self.assertTrue(landed, "the push didn't land in its lane")

    def test_exec_carries_bytes_and_status(self) -> None:
        self.assertEqual(len(self.in_bottle("head", "-c", "50000000", "/dev/urandom").stdout), 50_000_000)
        self.assertEqual(self.in_bottle("wc", "-c", input=bytes(20_000_000)).stdout.strip(), b"20000000")
        self.assertEqual(self.in_bottle("sh", "-c", "exit 7").returncode, 7)

    def test_the_network_after_a_restart_and_after_folio_serve_restarts(self) -> None:
        self.bottle("stop", self.machine)
        self.assertEqual(self.https_status(), "200")  # folio exec starts it again, with its policy
        self.folio("serve", "stop", check=True)
        self.out("git", "-C", "/workspace", "fetch", "-q", "origin")   # folio serve, started again, answers it
        self.assertIn(self.machine, self.log.read_text())


if __name__ == "__main__":
    unittest.main()
