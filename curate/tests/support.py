"""Shared harness: a throwaway repository, and the tool under test.

See ../README.md for what the tool does and ../CONTRIBUTING.md for the
conventions here.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

# This checkout's launcher, or the one CURATE_BIN names. Never PATH: tests that
# silently exercise whatever curate you happen to have installed prove nothing
# about the one you are changing.
#
# Absolute, because the tool runs with cwd set to a throwaway repository, so a
# relative path would be looked for in there.
CURATE = os.path.abspath(
    os.environ.get("CURATE_BIN")
    or Path(__file__).resolve().parent.parent.parent / "bin" / "curate")
HAVE_CURATE = os.path.isfile(CURATE)


class Repo:
    """A throwaway git repository, driven with plumbing rather than checkouts.

    Histories are built with commit-tree and update-index so the tests say
    exactly what they mean and never depend on what happens to be checked out.
    """

    def __init__(self, path: Path):
        self.path = path
        self.git("init", "-q", "-b", "wip", ".")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.com")

    def git(self, *args: str, check: bool = True, env: dict[str, str] | None = None) -> str:
        result = subprocess.run(
            ["git", "-C", str(self.path), *args],
            capture_output=True, text=True,
            env={**os.environ, **(env or {})},
        )
        if check and result.returncode != 0:
            raise AssertionError(f"git {' '.join(args)} failed:\n{result.stderr}")
        return result.stdout.strip()

    def git_raw(self, *args: str) -> str:
        """git output verbatim -- patches and other things a strip() would corrupt."""
        return subprocess.run(["git", "-C", str(self.path), *args],
                              capture_output=True, text=True, check=True).stdout

    def curate(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([CURATE, *args], cwd=self.path, capture_output=True, text=True)

    def status_fields(self) -> dict[str, str]:
        out = self.curate("status", "--porcelain")
        assert out.returncode == 0, out.stderr
        return dict(line.split("=", 1) for line in out.stdout.splitlines() if "=" in line)

    # --- building content -----------------------------------------------------

    def write(self, name: str, text: str) -> None:
        (self.path / name).write_text(text)

    def read(self, name: str) -> str:
        return (self.path / name).read_text()

    def blob(self, text: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.path), "hash-object", "-w", "--stdin"],
            input=text, capture_output=True, text=True,
        ).stdout.strip()

    def commit_all(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def stage_hunk(self, path: str, old: str, new: str) -> None:
        """Approve part of a file: what `git apply --cached` does for one hunk."""
        # --add because approving a file the working line added means adding it
        # to the review index; it is not there yet. Harmless when it is.
        self.git("update-index", "--add", "--cacheinfo",
                 f"100644,{self.blob(new)},{path}")

    # --- inspecting -----------------------------------------------------------

    def tree_of(self, rev: str) -> str:
        return self.git("rev-parse", f"{rev}^{{tree}}")

    def index_tree(self, index: str | None = None) -> str:
        env = {"GIT_INDEX_FILE": index} if index else {}
        return self.git("write-tree", env=env)

    def log(self, rev: str) -> list[str]:
        out = self.git("log", "--format=%s", rev)
        return out.splitlines() if out else []

    def short_status(self) -> list[str]:
        # git_raw, not git: the first column is a space when something is
        # unstaged, and strip() eats it off the first line.
        out = self.git_raw("status", "--short")
        return out.splitlines() if out else []

    def file_at(self, rev: str, path: str) -> str:
        return self.git_raw("cat-file", "blob", f"{rev}:{path}")

    def mtimes(self) -> dict[str, float]:
        return {
            str(p.relative_to(self.path)): p.stat().st_mtime
            for p in self.path.rglob("*")
            if p.is_file() and ".git" not in p.parts
        }


@unittest.skipUnless(HAVE_CURATE, f"{CURATE} not found; set CURATE_BIN to the tool under test")


class CurateTestCase(unittest.TestCase):
    """A repository with a working line ahead of a base, ready to review."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name) / "r"
        root.mkdir(parents=True)
        self.repo = Repo(root)

    def make_base(self) -> str:
        self.repo.write("f.txt", "one\ntwo\nthree\n")
        return self.repo.commit_all("base")

    def start(self, base: str) -> None:
        result = self.repo.curate("start", "--from", base)
        self.assertEqual(result.returncode, 0, result.stderr)
