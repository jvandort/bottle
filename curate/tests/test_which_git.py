"""gitcmd.which_git: past Apple's /usr/bin/git shim to the git it would start, with no process."""

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from curate.gitcmd import which_git  # noqa: E402

SHIM = Path("/usr/bin/git")


@unittest.skipUnless(SHIM.exists(), "needs a /usr/bin/git to stand in for the shim")
class WhichGit(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.developer = self.tmp / "Developer"
        self.real = self.developer / "usr" / "bin" / "git"
        self.real.parent.mkdir(parents=True)
        self.real.write_text("#!/bin/sh\n")
        self.real.chmod(0o755)
        self.link = self.tmp / "xcode_select_link"
        self.link.symlink_to(self.developer)

    def which(self, path: str = "/usr/bin", platform: str = "darwin", **environ: str) -> str:
        return which_git(path=path, environ=environ, platform=platform, selected=str(self.link))

    def test_behind_the_shim_the_selected_developer_directorys_git(self) -> None:
        self.assertEqual(str(self.real), self.which())

    def test_developer_dir_wins_as_it_does_for_xcrun(self) -> None:
        other = self.tmp / "Other" / "usr" / "bin" / "git"
        other.parent.mkdir(parents=True)
        other.write_text("#!/bin/sh\n")
        other.chmod(0o755)
        self.assertEqual(str(other), self.which(DEVELOPER_DIR=str(self.tmp / "Other")))

    def test_no_real_git_there_keeps_the_shim(self) -> None:
        self.real.unlink()
        self.assertEqual(str(SHIM), self.which())

    def test_another_git_first_on_path_is_the_one(self) -> None:
        mine = self.tmp / "bin" / "git"
        mine.parent.mkdir()
        mine.write_text("#!/bin/sh\n")
        mine.chmod(0o755)
        self.assertEqual(str(mine), self.which(path=f"{mine.parent}{os.pathsep}/usr/bin"))

    def test_only_on_a_mac(self) -> None:
        self.assertEqual(str(SHIM), self.which(platform="linux"))

    def test_no_git_at_all_is_plain_git(self) -> None:
        self.assertEqual("git", self.which(path=str(self.tmp / "nowhere")))


if __name__ == "__main__":
    unittest.main()
