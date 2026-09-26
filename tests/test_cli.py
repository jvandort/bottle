import contextlib
import io
import unittest

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
        code, out, _ = self.run_cli("wrap", str(repo))
        self.assertEqual(code, 0)
        self.assertIn("Wrapped project:", out)

    def test_name_and_path(self) -> None:
        repo = self.make_repo()
        code, out, _ = self.run_cli("wrap", "named", str(repo))
        self.assertEqual(code, 0)
        self.assertIn("Wrapped named:", out)

    def test_rerun_reports_already_wrapped(self) -> None:
        repo = self.make_repo()
        self.run_cli("wrap", str(repo))
        code, out, _ = self.run_cli("wrap", str(repo))
        self.assertEqual(code, 0)
        self.assertEqual(out, f"Already wrapped project: {repo}\n")

    def test_errors_exit_nonzero_without_traceback(self) -> None:
        code, _, err = self.run_cli("wrap", str(self.tmp / "nope"))
        self.assertEqual(code, 1)
        self.assertEqual(err, f"bottle: error: {self.tmp / 'nope'} is not a directory\n")

    def test_too_many_arguments(self) -> None:
        code, _, err = self.run_cli("wrap", "a", "b", "c")
        self.assertEqual(code, 2)
        self.assertIn("takes [NAME] PATH", err)



class BuildCommandTest(unittest.TestCase):
    def test_image_is_required(self) -> None:
        err = io.StringIO()
        with contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as exit_:
            main(["build"])
        self.assertEqual(exit_.exception.code, 2)
        self.assertIn("the following arguments are required: image", err.getvalue())


if __name__ == "__main__":
    unittest.main()
