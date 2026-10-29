"""gitcmd.Refs: a snapshot of the refs that answers name questions as git would."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from .support import TIMEOUT, Repo

PACKAGE = Path(__file__).resolve().parent.parent
# Run where the repository is, in a process of its own: Refs reads the
# repository it is in, and a test may not move this process (see CONTRIBUTING).
ASK = """
import json, sys
sys.path.insert(0, sys.argv[1])
from curate.gitcmd import Refs
refs = Refs()
print(json.dumps({name: [refs.full(name), refs.abbrev(name)] for name in sys.argv[2:]}))
"""


class AnswersAsGitWould(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        (root / "r").mkdir()
        self.repo = Repo(root / "r")
        self.repo.write("f.txt", "x\n")
        self.repo.commit_all("base")
        git = self.repo.git
        git("branch", "main")
        git("branch", "both")
        git("tag", "both")                                    # a branch and a tag: ambiguous
        git("tag", "v1")
        for remote in ("origin", "bottle/dm"):                # a remote named inside a path
            git("init", "-q", "--bare", str(root / remote.replace("/", "-")))
            git("remote", "add", remote, str(root / remote.replace("/", "-")))
            git("push", "-q", remote, "wip", "main")
            git("fetch", "-q", remote)
        git("remote", "set-head", "origin", "main")           # origin/HEAD, a symbolic ref
        git("branch", "-q", "--set-upstream-to", "bottle/dm/wip", "wip")
        git("branch", "-q", "--set-upstream-to", "origin/main", "main")

    def test_every_answer_is_gits(self) -> None:
        names = ["wip", "main", "both", "v1", "origin/main", "origin/HEAD", "bottle/dm/wip", "bottle/dm/main",
                 "refs/heads/wip", "refs/remotes/origin/main", "wip@{upstream}", "main@{upstream}",
                 "both@{upstream}", "v1@{upstream}", "nope", "nope@{upstream}", "HEAD", "origin"]
        answers = json.loads(subprocess.run(
            [sys.executable, "-c", ASK, str(PACKAGE), *names], cwd=self.repo.path,
            capture_output=True, text=True, check=True, timeout=TIMEOUT).stdout)
        for name in names:
            for flag, mine in zip(("--symbolic-full-name", "--abbrev-ref"), answers[name]):
                with self.subTest(name=name, flag=flag):
                    git = subprocess.run(["git", "rev-parse", flag, name], cwd=self.repo.path,
                                         capture_output=True, text=True, timeout=TIMEOUT)
                    self.assertEqual(git.stdout.strip(), mine)


if __name__ == "__main__":
    unittest.main()
