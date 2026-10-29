"""`mypy --strict` passes, or the suite fails: a type error is a test failure, not a suggestion."""

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from .support import TIMEOUT

PROJECT = Path(__file__).resolve().parent.parent


@unittest.skipUnless(importlib.util.find_spec("mypy"), "mypy isn't installed: `pip install mypy` to check types")
class Types(unittest.TestCase):
    def test_mypy_strict_finds_nothing(self) -> None:
        # A cache of its own, so it shares no file with anything else running (see CONTRIBUTING).
        with tempfile.TemporaryDirectory() as cache:
            result = subprocess.run([sys.executable, "-m", "mypy", "--strict", "--cache-dir", cache, "."],
                                    cwd=PROJECT, capture_output=True, text=True, timeout=TIMEOUT)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
