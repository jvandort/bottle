import unittest
from pathlib import Path

from folio import repos
from tests.support import FolioTestCase


class ExecTest(FolioTestCase):
    """`folio exec`: a command in a thread's bottle, its exit status folio's."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.bottle = f"{repos.current(self.repo).key}-fix-it"

    def workspace(self) -> Path:
        return Path(self.fake()["bottles"][self.bottle]["mounts"][0]["dir"])

    def test_it_runs_in_the_bottle_and_exits_as_the_command_does(self) -> None:
        self.ok("exec", "fix-it", "--", "sh", "-c", "echo made > /workspace/made")
        self.assertEqual((self.workspace() / "made").read_text(), "made\n")
        failed = self.folio("exec", "fix-it", "sh", "-c", "exit 3")
        self.assertEqual(failed.returncode, 3)

    def test_a_terminal_is_given_when_its_asked_for(self) -> None:
        self.ok("exec", "-t", "fix-it", "--", "true")
        self.assertEqual(self.calls("exec")[-1][:2], ["exec", "-t"])

    def test_it_needs_a_command(self) -> None:
        refused = self.folio("exec", "fix-it")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("give the command", refused.stderr)

    def test_a_thread_without_a_sidecar_has_none_to_run_in(self) -> None:
        refused = self.folio("exec", "--sidecar", "fix-it", "--", "true")
        self.assertIn("has no sidecar", refused.stderr)


class CopyTest(FolioTestCase):
    """`folio cp`: into and out of a thread's bottle, as `cp -r` would."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        bottle = f"{repos.current(self.repo).key}-fix-it"
        self.workspace = Path(self.fake()["bottles"][bottle]["mounts"][0]["dir"])

    def test_out_of_a_bottle_and_into_one(self) -> None:
        (self.workspace / "report.txt").write_text("a report")
        out = self.tmp / "out"
        out.mkdir()
        self.assertEqual(self.ok("cp", "fix-it:report.txt", str(out)).strip(), str(out / "report.txt"))
        self.assertEqual((out / "report.txt").read_text(), "a report")

        tree = self.tmp / "fixtures"
        (tree / "sub").mkdir(parents=True)
        (tree / "sub" / "f").write_text("a fixture")
        self.assertEqual(self.ok("cp", str(tree), "fix-it:").strip(), "fix-it:/workspace/fixtures")
        self.assertEqual((self.workspace / "fixtures" / "sub" / "f").read_text(), "a fixture")

    def test_one_side_names_a_thread(self) -> None:
        refused = self.folio("cp", "a", "b")
        self.assertIn("name one, THREAD:PATH", refused.stderr)


class CopyCommandsTest(unittest.TestCase):
    """A copy is one bottle-cli cp, if both sides are one bottle-cli's; else one's tar piped to the other's."""

    def test_one_cp_on_one_bottle_cli_else_a_pipe_of_two(self) -> None:
        from folio import engine
        from folio.bottle import krun

        here, box, other = krun.LOCAL, krun.RunnerCli("box"), krun.RunnerCli("other")
        self.assertEqual(engine.copy_commands((here, "./a"), (here, "m:b")), [[krun.binary(), "cp", "./a", "m:b"]])
        self.assertEqual(engine.copy_commands((box, "m:a"), (box, "n:b")),
                         [[krun.folio_binary(), "on", "box", "cp", "m:a", "n:b"]])
        self.assertEqual(engine.copy_commands((box, "m:a"), (here, "b")),
                         [[krun.folio_binary(), "on", "box", "cp", "m:a", "-"], [krun.binary(), "cp", "-", "b"]])
        self.assertEqual(engine.copy_commands((box, "m:a"), (other, "n:b"))[1],
                         [krun.folio_binary(), "on", "other", "cp", "-", "n:b"])
