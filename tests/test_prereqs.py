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
    """bottle-cli is built from the repo's sources by `cargo xtask dist` when it's missing or they've changed since."""

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
                                    (prereqs.platform, "machine", lambda: "arm64"),
                                    (prereqs, "RUSTUP_CARGO", self.repo / "no-rustup" / "cargo")):
            patcher = mock.patch.object(target, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        env = mock.patch.dict(os.environ)
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop("BOTTLE_CLI", None)
        os.environ.pop("BOTTLE_DIST", None)

    def run_(self, cmd, cwd=None, stdout=None, env=None):
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
        self.assertEqual(self.commands, [["/bin/cargo", "xtask", "dist", "--out", str(self.built.parent)]])

    def test_silent_when_current(self) -> None:
        self.ensure()
        self.commands.clear()
        self.assertEqual(self.ensure(), "")
        self.assertEqual(self.commands, [])

    def test_built_again_when_a_source_changes(self) -> None:
        self.ensure()
        later = time.time() + 10
        os.utime(self.source, (later, later))  # touched, not changed
        self.ensure()
        self.assertEqual(len(self.commands), 1)
        self.source.write_text("fn main() { }\n")
        self.ensure()
        self.assertEqual(len(self.commands), 2)
        self.source.write_text("fn main() {}\n")  # back to what was built once before: built again all the same
        self.ensure()
        self.assertEqual(len(self.commands), 3)

    def test_what_cargo_builds_in_target_isnt_a_source(self) -> None:
        self.ensure()
        (self.repo / "cli" / "target").mkdir()
        (self.repo / "cli" / "target" / "x").write_text("")
        self.ensure()
        self.assertEqual(len(self.commands), 1)

    def test_one_named_is_used_as_it_is(self) -> None:
        os.environ["BOTTLE_CLI"] = str(self.repo / "mine")
        with self.assertRaisesRegex(BottleError, "BOTTLE_CLI is .*mine, which isn't there"):
            self.ensure()
        (self.repo / "mine").write_text("")
        self.ensure()
        self.assertEqual(self.commands, [])

    def test_where_dist_says(self) -> None:
        os.environ["BOTTLE_DIST"] = str(self.repo / "elsewhere")
        self.ensure()
        self.assertEqual(self.commands[0][-1], str(self.repo / "elsewhere"))
        self.assertEqual(krun.binary(), str(self.repo / "elsewhere" / "bottle-cli"))

    def test_rustups_cargo_when_its_not_on_path(self) -> None:
        # As in a non-interactive ssh session, whose shell never read rustup's addition to PATH.
        prereqs.RUSTUP_CARGO.parent.mkdir()
        prereqs.RUSTUP_CARGO.write_text("")
        self.ensure(cargo=False)
        self.assertEqual(self.commands[0][0], str(prereqs.RUSTUP_CARGO))

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
