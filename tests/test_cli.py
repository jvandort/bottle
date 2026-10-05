import contextlib
import io
import logging
import sys
import unittest
from unittest import mock

from bottle.cli import main
from tests.support import GitTestCase


class WrapCommandTest(GitTestCase):
    def run_cli(self, *argv: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = main(list(argv))
            except SystemExit as e:
                code = e.code
        return code, out.getvalue(), err.getvalue()

    def test_path_only(self) -> None:
        repo = self.make_repo()
        code, out, _ = self.run_cli("repo", "add", str(repo))
        self.assertEqual(code, 0)
        self.assertEqual(out, f"Created repo 'project' ({repo}) with features []\n")

    def test_name_and_path(self) -> None:
        repo = self.make_repo()
        code, out, _ = self.run_cli("repo", "add", "named", str(repo))
        self.assertEqual(code, 0)
        self.assertIn("Created repo 'named'", out)

    def test_rerun_reports_already_wrapped(self) -> None:
        repo = self.make_repo()
        self.run_cli("repo", "add", str(repo))
        code, out, _ = self.run_cli("repo", "add", str(repo))
        self.assertEqual(code, 0)
        self.assertEqual(out, f"Repo 'project' already exists ({repo}) with features []\n")

    def test_errors_exit_nonzero_without_traceback(self) -> None:
        code, _, err = self.run_cli("repo", "add", str(self.tmp / "nope"))
        self.assertEqual(code, 1)
        self.assertEqual(err, f"bottle: error: {self.tmp / 'nope'} is not a directory\n")

    def test_add_with_features_then_list_and_set(self) -> None:
        repo = self.make_repo()
        self.run_cli("repo", "add", str(repo), "--feature", "tools", "--feature", "jvm:version=17")
        _, out, _ = self.run_cli("repo", "list")
        self.assertEqual(out.splitlines()[1].split(), ["project", str(repo), "tools", "jvm:version=17"])
        code, out, _ = self.run_cli("repo", "set", "project")
        self.assertEqual((code, out), (0, "Set repo 'project' features to []\n"))

    def test_update_adds_to_existing_features(self) -> None:
        repo = self.make_repo()
        self.run_cli("repo", "add", str(repo), "--feature", "tools")
        code, out, _ = self.run_cli("repo", "update", "project", "--feature", "claude")
        self.assertEqual((code, out), (0, "Set repo 'project' features to [claude, tools]\n"))

    def test_update_requires_a_feature(self) -> None:
        repo = self.make_repo()
        self.run_cli("repo", "add", str(repo))
        code, _, err = self.run_cli("repo", "update", "project")
        self.assertEqual(code, 2)
        self.assertIn("required: --feature", err)

    def test_too_many_arguments(self) -> None:
        code, _, err = self.run_cli("repo", "add", "a", "b", "c")
        self.assertEqual(code, 2)
        self.assertIn("takes [NAME] PATH", err)


class LogTest(WrapCommandTest):
    def log(self) -> str:
        return (self.bottle_home / "logs" / "bottle.log").read_text()

    def test_each_command_and_how_it_went_are_logged(self) -> None:
        repo = self.make_repo()
        self.run_cli("repo", "add", str(repo))
        self.run_cli("repo", "add", str(self.tmp / "nope"))
        log = self.log()
        self.assertRegex(log, rf"\d+ bottle.cli bottle repo add {repo}\n")
        self.assertRegex(log, rf"bottle repo add {repo}: exit 0 after [\d.]+s")
        self.assertRegex(log, rf"bottle repo add {self.tmp / 'nope'}: failed after [\d.]+s: .* is not a directory")

    def test_the_daemon_logs_to_its_own_file(self) -> None:
        with mock.patch("bottle.daemon.serve", new=mock.AsyncMock()), mock.patch.object(sys, "excepthook"):
            self.run_cli("daemon", "start", "--foreground")
        bottled = (self.bottle_home / "logs" / "bottled.log").read_text()
        self.assertRegex(bottled, r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d+ \d+ bottle.cli bottle daemon start --foreground\n")
        self.assertFalse((self.bottle_home / "logs" / "bottle.log").exists())

    def test_python_and_asyncio_reports_are_stamped_too(self) -> None:
        self.run_cli("repo", "list")
        logging.getLogger("asyncio").warning("Task was destroyed but it is pending!")
        self.assertRegex(self.log(), r"\n\d{4}-\d\d-\d\d [\d:,]+ \d+ asyncio Task was destroyed")

    def test_only_the_owner_can_read_the_log_or_list_bottle_home(self) -> None:
        self.bottle_home.mkdir(mode=0o755)
        (self.bottle_home / "logs").mkdir(mode=0o755)
        (self.bottle_home / "logs" / "bottle.log").touch(mode=0o644)
        self.run_cli("repo", "list")
        for path, mode in ((self.bottle_home, 0o700), (self.bottle_home / "logs", 0o700),
                           (self.bottle_home / "logs" / "bottle.log", 0o600)):
            self.assertEqual(path.stat().st_mode & 0o777, mode, path)


class BuildCommandTest(unittest.TestCase):
    def test_image_defaults_to_base(self) -> None:
        with mock.patch("bottle.features.build", return_value="bottle/base:latest") as build, \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(main(["build", "--feature", "tools"]), 0)
        build.assert_called_once_with("base", ["tools"], False)


if __name__ == "__main__":
    unittest.main()
