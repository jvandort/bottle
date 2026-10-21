import contextlib
import io
import logging
import sys
import unittest
from unittest import mock

from bottle.cli import main
from tests.support import GitTestCase


class CliTestCase(GitTestCase):
    def run_cli(self, *argv: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = main(list(argv))
            except SystemExit as e:
                code = e.code
        return code, out.getvalue(), err.getvalue()

    def test_errors_exit_nonzero_without_traceback(self) -> None:
        code, _, err = self.run_cli("spec", "validate", str(self.tmp / "nope.json"))
        self.assertEqual(code, 1)
        self.assertEqual(err, f"bottle: error: can't read spec {self.tmp / 'nope.json'}: No such file or directory\n")


class LogTest(CliTestCase):
    def log(self) -> str:
        return (self.bottle_home / "logs" / "bottle.log").read_text()

    def test_each_command_and_how_it_went_are_logged(self) -> None:
        spec = self.tmp / "spec.json"
        spec.write_text('{"features": []}')
        self.run_cli("spec", "validate", str(spec))
        self.run_cli("spec", "validate", str(self.tmp / "nope.json"))
        log = self.log()
        self.assertRegex(log, rf"\d+ bottle.cli bottle spec validate {spec}\n")
        self.assertRegex(log, rf"bottle spec validate {spec}: exit 0 after [\d.]+s")
        self.assertRegex(log, rf"bottle spec validate {self.tmp / 'nope.json'}: failed after [\d.]+s: can't read spec")

    def test_the_daemon_logs_to_its_own_file(self) -> None:
        with mock.patch("bottle.daemon.serve", new=mock.AsyncMock()), mock.patch.object(sys, "excepthook"):
            self.run_cli("daemon", "start", "--foreground")
        bottled = (self.bottle_home / "logs" / "bottled.log").read_text()
        self.assertRegex(bottled, r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d+ \d+ bottle.cli bottle daemon start --foreground\n")
        self.assertFalse((self.bottle_home / "logs" / "bottle.log").exists())

    def test_python_and_asyncio_reports_are_stamped_too(self) -> None:
        self.run_cli("status")
        logging.getLogger("asyncio").warning("Task was destroyed but it is pending!")
        self.assertRegex(self.log(), r"\n\d{4}-\d\d-\d\d [\d:,]+ \d+ asyncio Task was destroyed")

    def test_only_the_owner_can_read_the_log_or_list_bottle_home(self) -> None:
        self.bottle_home.mkdir(mode=0o755)
        (self.bottle_home / "logs").mkdir(mode=0o755)
        (self.bottle_home / "logs" / "bottle.log").touch(mode=0o644)
        self.run_cli("status")
        for path, mode in ((self.bottle_home, 0o700), (self.bottle_home / "logs", 0o700),
                           (self.bottle_home / "logs" / "bottle.log", 0o600)):
            self.assertEqual(path.stat().st_mode & 0o777, mode, path)


class BuildCommandTest(unittest.TestCase):
    def test_image_defaults_to_base(self) -> None:
        with mock.patch("bottle.features.build", return_value="bottle/base:latest") as build, \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(main(["build", "--feature", "tools"]), 0)
        build.assert_called_once_with("base", ["tools"], False)


class SpecCommandsTest(CliTestCase):

    def spec(self, data: str) -> str:
        path = self.tmp / "spec.json"
        path.write_text(data)
        return str(path)

    def test_validate_prints_the_canonical_spec(self) -> None:
        spec = self.spec('{"features": ["tools", "jvm:version=25"]}')
        self.assertEqual(self.run_cli("spec", "validate", spec), (0, "Valid: features [jvm, tools] and memory all\n", ""))
        self.assertEqual(self.run_cli("spec", "validate", "--json", spec),
                         (0, '{"features": ["jvm", "tools"], "memory": "all"}\n', ""))

    def test_validate_fails_with_bottles_own_message(self) -> None:
        spec = self.spec('{"features": ["nope"]}')
        code, out, err = self.run_cli("spec", "validate", "--json", spec)
        self.assertEqual((code, out), (1, ""))
        self.assertIn("no feature named 'nope'", err)

    def test_new_needs_a_spec_and_a_name(self) -> None:
        spec = self.spec('{"features": []}')
        for argv, message in (
            ((), "the following arguments are required: --spec, --name"),
            (("--spec", spec), "the following arguments are required: --name"),
            (("--name", "b"), "the following arguments are required: --spec"),
            (("repo", "--spec", spec, "--name", "b"), "unrecognized arguments: repo"),
            (("--spec", spec, "--name", "b", "--feature", "tools"), "unrecognized arguments: --feature"),
        ):
            with self.subTest(argv):
                code, _, err = self.run_cli("new", *argv)
                self.assertEqual(code, 2)
                self.assertIn(message, err)

    def test_new_from_a_spec(self) -> None:
        spec = self.spec('{"features": ["tools"]}')
        context = self.tmp / "context.md"
        context.write_text("More.\n")
        repo = self.make_repo()
        bottle = mock.Mock(git_repo=str(repo), branch="main", commit="c" * 40, features=("tools",), memory=None)
        bottle.name = "project"
        with mock.patch("bottle.bottles.create", return_value=bottle) as create:
            code, out, _ = self.run_cli("new", "--spec", spec, "--git-repo", str(repo), "--git-remote", "origin:fetch=upstream",
                                        "--checkout", "main", "--name", "project", "--context", str(context))
        self.assertEqual(code, 0)
        create.assert_called_once_with({"features": ["tools"], "memory": "all"}, "project", repo, ["origin:fetch=upstream"],
                                       "main", "More.\n")
        self.assertIn(f"Created bottle 'project', with {repo} at main (cccccccccccc)", out)

    def test_status_shows_missing_bottles(self) -> None:
        self.assertEqual(self.run_cli("status", "gone", "--json"), (0, '[\n  {\n    "name": "gone",\n    "state": "missing"\n  }\n]\n', ""))
        code, out, _ = self.run_cli("status", "gone")
        self.assertEqual(out.splitlines()[1].split(), ["gone", "-", "-", "-", "-", "missing"])


if __name__ == "__main__":
    unittest.main()
