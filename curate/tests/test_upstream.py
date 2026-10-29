"""The working line follows its upstream while you read: a fetch that moves
it brings your files along, as unread, and leaves your approvals alone."""

import subprocess

from .support import CurateTestCase, TIMEOUT


class FollowUpstream(CurateTestCase):
    """`wip` tracks origin/wip; someone else pushes to origin; you fetch while reading."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("first")
        self.origin = self.repo.path.parent / "origin.git"
        self.repo.git("clone", "-q", "--bare", str(self.repo.path), str(self.origin))
        self.repo.git("remote", "add", "origin", str(self.origin))
        self.repo.git("fetch", "-q", "origin")
        self.repo.git("branch", "-q", "--set-upstream-to", "origin/wip")
        self.start(self.base)
        self.assertEqual(self.repo.curate("read").returncode, 0)
        self.repo.git("add", "f.txt")                          # approved
        self.elsewhere = self.repo.path.parent / "elsewhere"
        self.repo.git("clone", "-q", str(self.origin), str(self.elsewhere))

    def push(self, name: str, text: str) -> str:
        (self.elsewhere / name).write_text(text)
        for args in (["add", name], ["-c", "user.name=T", "-c", "user.email=t@t", "commit", "-q", "-m", name],
                     ["push", "-q", "origin", "HEAD:wip"]):
            subprocess.run(["git", "-C", str(self.elsewhere), *args], check=True, capture_output=True, timeout=TIMEOUT)
        return subprocess.run(["git", "-C", str(self.elsewhere), "rev-parse", "HEAD"], capture_output=True,
                              text=True, check=True, timeout=TIMEOUT).stdout.strip()

    def fetch(self) -> str:
        result = subprocess.run(["git", "fetch", "-q", "origin"], cwd=self.repo.path, capture_output=True,
                                text=True, timeout=TIMEOUT)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stderr

    def test_a_fetch_brings_the_working_line_and_your_files_along_as_unread(self) -> None:
        pushed = self.push("g.txt", "new\n")
        said = self.fetch()
        self.assertEqual(pushed, self.repo.git("rev-parse", "wip"))
        self.assertEqual("new\n", self.repo.read("g.txt"))
        self.assertEqual("refs/heads/curate/wip", self.repo.git("symbolic-ref", "HEAD"))
        self.assertEqual(sorted(["M  f.txt", "?? g.txt"]), sorted(self.repo.short_status()))
        self.assertIn("'wip' followed origin/wip", said)

    def test_write_mode_finds_the_new_tip_clean(self) -> None:
        self.push("g.txt", "new\n")
        self.fetch()
        self.assertEqual(self.repo.curate("write").returncode, 0)
        self.assertEqual([], self.repo.short_status())
        self.assertEqual(self.repo.curate("read").returncode, 0)
        self.assertIn("M  f.txt", self.repo.short_status())     # the approval came back

    def test_not_over_an_edit_in_the_way(self) -> None:
        tip = self.repo.git("rev-parse", "wip")
        self.repo.write("f.txt", "ONE\ntwo\nmine\n")
        self.push("f.txt", "ONE\nTWO\nthree\n")
        said = self.fetch()
        self.assertEqual(tip, self.repo.git("rev-parse", "wip"))
        self.assertEqual("ONE\ntwo\nmine\n", self.repo.read("f.txt"))
        self.assertIn("stayed where it was", said)

    def test_not_over_commits_the_working_line_has_and_upstream_doesnt(self) -> None:
        self.repo.curate("write")
        self.repo.write("mine.txt", "mine\n")
        mine = self.repo.commit_all("mine")
        self.repo.curate("read")
        self.push("g.txt", "new\n")
        said = self.fetch()
        self.assertEqual(mine, self.repo.git("rev-parse", "wip"))
        self.assertIn("curate git pull", said)

    def test_write_mode_is_left_to_you(self) -> None:
        self.repo.curate("write")
        tip = self.repo.git("rev-parse", "wip")
        self.push("g.txt", "new\n")
        self.assertEqual("", self.fetch())
        self.assertEqual(tip, self.repo.git("rev-parse", "wip"))

    def test_the_hook_is_installed_with_the_others(self) -> None:
        hook = self.repo.path / ".git" / "hooks" / "reference-transaction"
        self.assertIn("installed by curate", hook.read_text())
