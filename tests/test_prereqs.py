import contextlib
import io
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from bottle import krun, prereqs
from bottle.errors import BottleError


class EnsureCliTest(unittest.TestCase):
    """bottle-cli is built from the repo's sources by `cargo xtask dist` when it's missing or older than they are."""

    def setUp(self) -> None:
        prereqs.ensure_cli.cache_clear()
        self.addCleanup(prereqs.ensure_cli.cache_clear)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.repo = Path(tmp.name)
        (self.repo / "cli" / "src").mkdir(parents=True)
        self.source = self.repo / "cli" / "src" / "main.rs"
        self.source.write_text("fn main() {}\n")
        self.built = self.repo / "target" / "dist" / "bottle-cli"
        self.commands: list[list[str]] = []
        for target, name, value in ((krun, "REPO", self.repo), (prereqs.platform, "system", lambda: "Darwin"),
                                    (prereqs.platform, "machine", lambda: "arm64")):
            patcher = mock.patch.object(target, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        env = mock.patch.dict(os.environ)
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop("BOTTLE_CLI", None)

    def run_(self, cmd, cwd=None, stdout=None):
        self.commands.append(cmd)
        self.built.parent.mkdir(parents=True, exist_ok=True)
        self.built.write_text("")
        return subprocess.CompletedProcess(cmd, 0)

    def ensure(self, cargo: bool = True) -> str:
        with mock.patch.object(prereqs.subprocess, "run", side_effect=self.run_), \
                mock.patch.object(prereqs.shutil, "which", return_value="/bin/cargo" if cargo else None), \
                contextlib.redirect_stderr(io.StringIO()) as stderr:
            prereqs.ensure_cli()
        prereqs.ensure_cli.cache_clear()
        return stderr.getvalue()

    def test_built_when_missing(self) -> None:
        self.assertIn("cargo xtask dist", self.ensure())
        self.assertEqual(self.commands, [["cargo", "xtask", "dist"]])

    def test_silent_when_current(self) -> None:
        self.ensure()
        self.commands.clear()
        self.assertEqual(self.ensure(), "")
        self.assertEqual(self.commands, [])

    def test_built_again_when_a_source_changes(self) -> None:
        self.ensure()
        later = time.time() + 10
        os.utime(self.source, (later, later))
        self.ensure()
        self.assertEqual(len(self.commands), 2)

    def test_what_cargo_builds_in_target_isnt_a_source(self) -> None:
        self.ensure()
        later = time.time() + 10
        (self.repo / "cli" / "target").mkdir()
        (self.repo / "cli" / "target" / "x").write_text("")
        os.utime(self.repo / "cli" / "target" / "x", (later, later))
        self.ensure()
        self.assertEqual(len(self.commands), 1)

    def test_one_named_is_used_as_it_is(self) -> None:
        os.environ["BOTTLE_CLI"] = str(self.repo / "mine")
        with self.assertRaisesRegex(BottleError, "BOTTLE_CLI is .*mine, which isn't there"):
            self.ensure()
        (self.repo / "mine").write_text("")
        self.ensure()
        self.assertEqual(self.commands, [])

    def test_needs_cargo(self) -> None:
        with self.assertRaisesRegex(BottleError, "rustup"):
            self.ensure(cargo=False)

    def test_checks_once_per_process(self) -> None:
        with mock.patch.object(prereqs.subprocess, "run", side_effect=self.run_), \
                mock.patch.object(prereqs.shutil, "which", return_value="/bin/cargo"), \
                contextlib.redirect_stderr(io.StringIO()):
            prereqs.ensure_cli()
            prereqs.ensure_cli()
        self.assertEqual(len(self.commands), 1)

    def test_rejects_intel(self) -> None:
        with mock.patch.object(prereqs.platform, "machine", return_value="x86_64"), \
                self.assertRaisesRegex(BottleError, "Apple silicon"):
            self.ensure()


if __name__ == "__main__":
    unittest.main()
