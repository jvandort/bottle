import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


class GitTestCase(unittest.TestCase):
    """Runs each test in a temp dir with BOTTLE_HOME and git config isolated.

    The user's global git config is ignored so tests never sign commits,
    prompt for passphrases, or pick up hooks and aliases.
    """

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        # Resolve so paths match what git reports (/var is a symlink on macOS).
        self.tmp = Path(tmp.name).resolve()
        self.bottle_home = self.tmp / "bottle-home"
        env = mock.patch.dict(
            os.environ,
            {
                "BOTTLE_HOME": str(self.bottle_home),
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_AUTHOR_NAME": "Test",
                "GIT_AUTHOR_EMAIL": "test@example.com",
                "GIT_COMMITTER_NAME": "Test",
                "GIT_COMMITTER_EMAIL": "test@example.com",
            },
        )
        env.start()
        self.addCleanup(env.stop)

    def make_repo(self, name: str = "project", origin: str | None = None) -> Path:
        repo = self.tmp / name
        run("git", "init", "-q", "-b", "main", repo)
        self.commit(repo, "initial")
        if origin:
            run("git", "-C", repo, "remote", "add", "origin", origin)
        return repo

    def commit(self, repo: Path, message: str) -> str:
        run("git", "-C", repo, "commit", "-q", "--allow-empty", "-m", message)
        return run("git", "-C", repo, "rev-parse", "HEAD")


def run(*args: str | Path) -> str:
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True, check=True)
    return result.stdout.strip()
