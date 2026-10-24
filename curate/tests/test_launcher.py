"""The entry point, and that `curate/curate/` stays importable.

The package sits inside a project directory of the same name. See
curate/__init__.py; these run the arrangements where that could bite.
"""

import os
import subprocess
from pathlib import Path

from .support import CURATE, CurateTestCase, TIMEOUT


class Launcher(CurateTestCase):
    def test_runs_from_the_repository_root_with_it_on_the_path(self) -> None:
        """Where the project directory could shadow the package.

        From the repository root, `curate` is also a directory with no
        __init__.py, which Python would take as a namespace package with no
        `cli` inside. The launcher putting its own directory first is what
        stops that, and this is the arrangement that proves it.
        """
        root = Path(CURATE).resolve().parent.parent
        result = subprocess.run(
            [CURATE, "--version"], cwd=str(root), capture_output=True, text=True,
            env={**os.environ, "PYTHONPATH": str(root)}, timeout=TIMEOUT)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("curate", result.stdout)

    def test_runs_from_an_unrelated_directory(self) -> None:
        result = subprocess.run([CURATE, "--version"], cwd=str(self.repo.path),
                                capture_output=True, text=True, timeout=TIMEOUT)
        self.assertEqual(result.returncode, 0, result.stderr)


class Bare(CurateTestCase):
    """`curate` with nothing after it answers "where am I?"."""

    def test_with_no_review_it_explains_itself(self) -> None:
        self.make_base()
        out = self.repo.curate().stdout
        self.assertIn("curate start --from <base>", out)
        self.assertIn("No review in this repository yet.", out)

    def test_in_a_review_it_is_status(self) -> None:
        self.start(self.make_base())
        self.assertEqual(self.repo.curate().stdout, self.repo.curate("status").stdout)

    def test_help_is_the_command_list(self) -> None:
        result = self.repo.curate("--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("curate start --from <base>", result.stdout)

    def test_outside_a_repository_it_says_so_and_nothing_else(self) -> None:
        outside = self.repo.path.parent / "not-a-repo"
        outside.mkdir()
        env = {**os.environ, "GIT_CEILING_DIRECTORIES": str(self.repo.path.parent)}
        bare = subprocess.run([CURATE], cwd=outside, capture_output=True,
                              text=True, env=env, timeout=TIMEOUT)
        self.assertEqual((bare.returncode, bare.stdout),
                         (0, "curate only works inside a git repository. Run it in one.\n"))
        verb = subprocess.run([CURATE, "status"], cwd=outside, capture_output=True,
                              text=True, env=env, timeout=TIMEOUT)
        self.assertEqual(verb.returncode, 1)
        self.assertIn("not a git repository", verb.stderr)
