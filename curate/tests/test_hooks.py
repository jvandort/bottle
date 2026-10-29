"""The hooks, and surviving the path inside them going stale.

A hook holds an absolute path to curate. Renaming the checkout, moving it, or
-- as happened -- turning the file it names into a directory leaves git running
something that is not curate, on every commit, until someone traces it.
"""

import subprocess

from .support import CurateTestCase, TIMEOUT


class Hooks(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.hooks = self.repo.path / ".git" / "hooks"

    def point_hooks_at(self, target: str) -> None:
        for name in ("post-commit", "post-index-change", "post-checkout"):
            path = self.hooks / name
            path.write_text("\n".join(
                f'CURATE="{target}"' if line.startswith("CURATE=") else line
                for line in path.read_text().split("\n")))

    def commit_something(self) -> subprocess.CompletedProcess[str]:
        self.repo.write("g.txt", "more\n")
        self.repo.git("add", "-A")
        return subprocess.run(["git", "-C", str(self.repo.path), "commit", "-m", "x"],
                              capture_output=True, text=True, timeout=TIMEOUT)

    def test_a_hook_naming_a_curate_that_is_gone_says_nothing(self) -> None:
        self.point_hooks_at(str(self.repo.path / "not-here"))
        result = self.commit_something()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")

    def test_nor_one_naming_a_directory(self) -> None:
        """The actual failure: `curate/curate` became a package."""
        directory = self.repo.path / "nowdir"
        directory.mkdir()
        self.point_hooks_at(str(directory))
        result = self.commit_something()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("__main__", result.stderr)
        self.assertEqual(result.stderr, "")

    def test_any_ordinary_command_repairs_a_stale_path(self) -> None:
        """`status` is deliberately not one: it reports rather than repairs."""
        self.point_hooks_at("/gone")
        self.assertEqual(self.repo.curate("list").returncode, 0)
        for name in ("post-commit", "post-index-change", "post-checkout"):
            with self.subTest(hook=name):
                self.assertNotIn('CURATE="/gone"', (self.hooks / name).read_text())

    def test_a_hook_that_is_not_ours_is_left_alone(self) -> None:
        mine = self.hooks / "post-commit"
        mine.write_text("#!/bin/sh\necho mine\n")
        self.repo.curate("list")
        self.assertEqual(mine.read_text(), "#!/bin/sh\necho mine\n")

    def test_a_hook_that_is_not_ours_is_reported_every_time(self) -> None:
        """Saying it once, at `start`, is not enough.

        git-lfs ships a post-checkout and a post-commit, so this is the
        ordinary case rather than the odd one, and what it costs -- approvals
        that are only durable at the next command -- is invisible until you
        need them.
        """
        (self.hooks / "post-index-change").write_text("#!/bin/sh\necho mine\n")
        self.assertEqual(self.repo.status_fields()["hooks"], "partial")
        report = self.repo.curate("status").stdout
        self.assertIn("post-index-change", report)
        self.assertIn('hook post-index-change "$@"', report,
                      "should say how to chain it")

    def test_start_says_which_hooks_of_yours_it_left_alone(self) -> None:
        self.repo.curate("drop", "wip")
        (self.hooks / "post-commit").write_text("#!/bin/sh\necho mine\n")
        result = self.repo.curate("start", "--from", self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("leaving your existing post-commit hook alone", result.stderr)

    def test_but_not_on_every_other_command(self) -> None:
        (self.hooks / "post-commit").write_text("#!/bin/sh\necho mine\n")
        for verb in ("list", "read", "write"):
            with self.subTest(verb=verb):
                self.assertNotIn("leaving your existing", self.repo.curate(verb).stderr)

    def test_commit_all_in_review_mode_is_recorded_at_once(self) -> None:
        """`git commit -a` stages into a temporary index, which the
        post-index-change hook leaves alone; post-commit records it instead."""
        self.repo.curate("read")
        self.repo.git("commit", "-q", "-a", "-m", "all of it")
        ref = self.repo.git("for-each-ref", "--format=%(refname)", "refs/curate")
        self.assertEqual(self.repo.tree_of(ref), self.repo.tree_of("curate/wip"))

    def test_switching_records_what_a_foreign_hook_kept_from_curate(self) -> None:
        """With someone else's post-index-change, an approval reaches the ref
        at the next command -- which has to be every command, a switch included."""
        (self.hooks / "post-index-change").write_text("#!/bin/sh\nexit 0\n")
        self.repo.curate("read")
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.curate("write")
        ref = self.repo.git("for-each-ref", "--format=%(refname)", "refs/curate")
        self.assertEqual(self.repo.tree_of(ref), approved)

    def test_hooks_are_ok_when_they_are_all_ours(self) -> None:
        self.assertEqual(self.repo.status_fields()["hooks"], "ok")
        self.assertEqual(self.repo.status_fields()["problems"], "0")
