import contextlib
import io
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

    def test_too_many_arguments(self) -> None:
        code, _, err = self.run_cli("repo", "add", "a", "b", "c")
        self.assertEqual(code, 2)
        self.assertIn("takes [NAME] PATH", err)



class BuildCommandTest(unittest.TestCase):
    def test_image_defaults_to_base(self) -> None:
        with mock.patch("bottle.features.build", return_value="bottle/base:latest") as build, \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(main(["build", "--feature", "tools"]), 0)
        build.assert_called_once_with("base", ["tools"], False)


if __name__ == "__main__":
    unittest.main()
