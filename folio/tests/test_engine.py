import subprocess
import unittest
from unittest import mock

from folio import engine
from folio.errors import FolioError


class EngineTest(unittest.TestCase):
    def setUp(self) -> None:
        # A bottle's command line, as bottles.py gives it: bottle-cli's exec, on its machine.
        patcher = mock.patch.object(engine.bottles, "exec_command",
                                    side_effect=lambda name, argv, tty=False: ["bottle-cli", "exec", name, "--", *argv])
        self.exec_command = patcher.start()
        self.addCleanup(patcher.stop)

    def run_with(self, returncode: int = 0, stdout: str = "", stderr: str = ""):
        result = subprocess.CompletedProcess([], returncode, stdout, stderr)
        return mock.patch("subprocess.run", return_value=result)

    def test_commands_never_get_this_terminal_for_stdin(self) -> None:
        """An exec hands stdin to the bottle, which would eat what's typed at folio's prompts."""
        with self.run_with() as run:
            engine.run("b", ["true"])
        self.assertIs(run.call_args.kwargs["stdin"], subprocess.DEVNULL)
        with self.run_with() as run:
            engine.run("b", ["cat"], input="data")
        self.assertEqual((run.call_args.kwargs["input"], run.call_args.kwargs["stdin"]), ("data", None))

    def test_a_command_runs_as_the_bottles_exec(self) -> None:
        with self.run_with() as run:
            engine.run("b", ["shpool", "version"])
        self.assertEqual(run.call_args.args[0], ["bottle-cli", "exec", "b", "--", "shpool", "version"])

    def test_a_bottles_failures_are_folios_saying_what_it_said(self) -> None:
        with mock.patch.object(engine.bottles, "status", side_effect=engine.BottleError("runner box is off")), \
                self.assertRaisesRegex(FolioError, r"^runner box is off$"):
            engine.status(["b"])
        with self.assertRaisesRegex(FolioError, "features is a list"):
            engine.validate_spec({"features": "tools"})
        with self.run_with(1) as run, self.assertRaisesRegex(FolioError, r"^`sh` in b failed \(exit 1\)$"):
            engine.run("b", ["sh", "-c", "exit 1"])
        self.assertIsNone(run.call_args.kwargs["stderr"])   # what it said, it said where it says it

    def test_a_failure_the_caller_expects_says_nothing(self) -> None:
        with self.run_with(1) as run:
            self.assertEqual(engine.run("b", ["shpool", "list"], check=False).returncode, 1)
        self.assertIs(run.call_args.kwargs["stderr"], subprocess.DEVNULL)

    def test_a_bottle_is_made_with_its_record_where_its_owner_says(self) -> None:
        with mock.patch.object(engine.bottles, "create") as create, mock.patch("sys.stderr"):
            engine.new("b", {"features": []}, [engine.Mount("/workspace", "/r", None, [])], "", owner="/t")
        self.assertEqual(create.call_args.kwargs["owner"], "/t")
        self.assertEqual(create.call_args.kwargs["runner"], "local")
        [mount] = create.call_args.args[2]
        self.assertEqual((mount.path, mount.remotes), ("/workspace", None))   # no remotes: bottle's default


if __name__ == "__main__":
    unittest.main()
