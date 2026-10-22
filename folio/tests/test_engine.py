import subprocess
import unittest
from unittest import mock

from folio import engine
from folio.errors import FolioError


class EngineTest(unittest.TestCase):
    def run_with(self, returncode: int = 0, stdout: str = "", stderr: str = ""):
        result = subprocess.CompletedProcess([], returncode, stdout, stderr)
        return mock.patch("subprocess.run", return_value=result)

    def test_commands_never_get_this_terminal_for_stdin(self) -> None:
        """`bottle exec` hands stdin to the bottle, which would eat what's typed at folio's prompts."""
        with self.run_with() as run:
            engine.run("b", ["true"])
        self.assertIs(run.call_args.kwargs["stdin"], subprocess.DEVNULL)
        with self.run_with() as run:
            engine.run("b", ["cat"], input="data")
        self.assertEqual((run.call_args.kwargs["input"], run.call_args.kwargs["stdin"]), ("data", None))

    def test_bottles_errors_are_its_own_never_captured(self) -> None:
        """Bottle's stderr reaches the user as bottle wrote it; folio says only which command failed."""
        with self.run_with(1) as run, self.assertRaisesRegex(FolioError, r"^`bottle status` failed \(exit 1\)$"):
            engine.status(["b"])
        self.assertIsNone(run.call_args.kwargs["stderr"])
        with self.run_with(1), self.assertRaisesRegex(FolioError, r"^`bottle spec validate` failed"):
            engine.validate_spec({})
        with self.run_with(1), self.assertRaisesRegex(FolioError, r"^`bottle exec b` failed"):
            engine.run("b", ["sh", "-c", "exit 1"])

    def test_a_failure_the_caller_expects_says_nothing(self) -> None:
        with self.run_with(1) as run:
            self.assertEqual(engine.run("b", ["shpool", "list"], check=False).returncode, 1)
        self.assertIs(run.call_args.kwargs["stderr"], subprocess.DEVNULL)

    def test_start_is_on_this_terminal(self) -> None:
        """Starting can need a login, which asks on the terminal."""
        with self.run_with() as run:
            engine.start("b")
        self.assertNotIn("stdin", run.call_args.kwargs)
        self.assertNotIn("capture_output", run.call_args.kwargs)
        with self.run_with(1), self.assertRaisesRegex(FolioError, r"`bottle start b` failed"):
            engine.start("b")

    def test_exec_puts_the_command_after_a_double_dash(self) -> None:
        with self.run_with() as run, mock.patch.object(engine, "command", return_value=["bottle"]):
            engine.run("b", ["shpool", "version"])
        self.assertEqual(run.call_args.args[0], ["bottle", "exec", "b", "--", "shpool", "version"])
