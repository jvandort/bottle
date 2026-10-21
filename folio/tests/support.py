import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

FOLIO = Path(__file__).resolve().parents[2] / "bin" / "folio"
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

    def folio(self, *args: str, cwd: Path | None = None, input: str | None = None) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(FOLIO), *args], cwd=cwd or self.repo, env=self.env,
                              capture_output=True, text=True, input=input)

    def ok(self, *args: str, cwd: Path | None = None) -> str:
        result = self.folio(*args, cwd=cwd)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def fake(self) -> dict:
        """The fake bottle's state: its bottles, and every call it got."""
        return json.loads(self.state.read_text()) if self.state.exists() else {"bottles": {}, "calls": []}

    def calls(self, command: str) -> list[list[str]]:
        return [call for call in self.fake()["calls"] if call[0] == command]


def git(*args: str) -> str:
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout.strip()
