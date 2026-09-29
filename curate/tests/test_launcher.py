"""The entry point, and that `curate/curate/` stays importable.

The package sits inside a project directory of the same name. See
curate/__init__.py; these run the arrangements where that could bite.
"""

import os
import subprocess
from pathlib import Path

from .support import CURATE, CurateTestCase


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
            env={**os.environ, "PYTHONPATH": str(root)},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("curate", result.stdout)

    def test_runs_from_an_unrelated_directory(self) -> None:
        result = subprocess.run([CURATE, "--version"], cwd=str(self.repo.path),
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
