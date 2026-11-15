import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# What the tests run as folio: $FOLIO_UNDER_TEST (tests/__init__.py makes it this checkout's debug build), else
# bin/folio.
FOLIO = Path(os.environ.get("FOLIO_UNDER_TEST") or Path(__file__).resolve().parents[2] / "bin" / "folio")
FAKE_BOTTLE = Path(__file__).resolve().parent / "fake_bottle.py"


class FolioTestCase(unittest.TestCase):
    """Each test in a temp dir: a repo to work in, FOLIO_HOME, the fake bottle, and git config isolated."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name).resolve()  # as git reports it: /var is a symlink on macOS
        self.home = self.tmp / "folio-home"
        self.state = self.tmp / "fake" / "state.json"
        self.state.parent.mkdir()
        self.env = {
            **os.environ,
            "FOLIO_HOME": str(self.home),
            # The stand-in bottle runs no bottle-cli.
            "BOTTLE_CLI": "/no/bottle-cli",
            "FOLIO_BOTTLE": str(FAKE_BOTTLE),
            "FAKE_BOTTLE_STATE": str(self.state),
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Test", "GIT_AUTHOR_EMAIL": "test@example.com",
            "GIT_COMMITTER_NAME": "Test", "GIT_COMMITTER_EMAIL": "test@example.com",
        }
        patcher = mock.patch.dict(os.environ, self.env)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.repo = self.make_repo("project")

    def make_repo(self, name: str) -> Path:
        repo = self.tmp / name
        git("init", "-q", "-b", "main", str(repo))
        self.commit(repo, "initial")
        return repo

    def commit(self, repo: Path, message: str) -> str:
        git("-C", str(repo), "commit", "-q", "--allow-empty", "-m", message)
        return git("-C", str(repo), "rev-parse", "HEAD")

    def folio(self, *args: str, cwd: Path | None = None, input: str | None = None,
              **env: str) -> subprocess.CompletedProcess:
        """folio with `args`, and `env` beside the test's."""
        # Never this terminal for stdin, even with no input: folio would think it had one to ask on, and wait.
        return subprocess.run([str(FOLIO), *args], cwd=cwd or self.repo, env={**self.env, **env},
                              capture_output=True, text=True, input=input or "")

    def ok(self, *args: str, cwd: Path | None = None) -> str:
        result = self.folio(*args, cwd=cwd)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def on_terminal(self, args: list[str], steps: list[tuple[bytes, bytes]], cwd: Path | None = None,
                    timeout: float = 20) -> tuple[int, bytes]:
        """folio with `args` on a pseudo-terminal, as a person runs it: for each step, once what folio has written
        has its first part, the second typed. Its exit status, and all it wrote."""
        import pty
        import select
        import time

        pid, fd = pty.fork()
        if pid == 0:  # the child: folio, on the terminal
            try:
                os.chdir(cwd or self.repo)
                os.execve(str(FOLIO), [str(FOLIO), *args],
                          {**self.env, "TERM": "xterm-256color", "LINES": "24", "COLUMNS": "80"})
            finally:
                os._exit(127)
        out, deadline, steps = b"", time.monotonic() + timeout, list(steps)
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([fd], [], [], 0.2)
                if ready:
                    try:
                        chunk = os.read(fd, 4096)
                    except OSError:
                        break  # the child has gone
                    if not chunk:
                        break
                    out += chunk
                while steps and steps[0][0] in out:
                    for key in steps.pop(0)[1]:
                        os.write(fd, bytes([key]))
                        time.sleep(0.02)
            _, status = os.waitpid(pid, os.WNOHANG) if time.monotonic() >= deadline else os.waitpid(pid, 0)
        finally:
            try:
                os.kill(pid, 9)
                os.waitpid(pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
            os.close(fd)
        return os.waitstatus_to_exitcode(status) if status else 0, out

    def fake(self) -> dict:
        """The fake bottle's state: its bottles, and every call it got."""
        return json.loads(self.state.read_text()) if self.state.exists() else {"bottles": {}, "calls": []}

    def calls(self, command: str) -> list[list[str]]:
        return [call for call in self.fake()["calls"] if call[0] == command]


def git(*args: str) -> str:
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout.strip()
