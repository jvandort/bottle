"""The hooks, and surviving the path inside them going stale.

A hook holds an absolute path to curate. Renaming the checkout, moving it, or
-- as happened -- turning the file it names into a directory leaves git running
something that is not curate, on every commit, until someone traces it.
"""

import subprocess

from .support import CurateTestCase


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
                              capture_output=True, text=True)

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
