from pathlib import Path

from tests import state as kept
from tests.support import FolioTestCase


class ExecTest(FolioTestCase):
    """`folio exec`: a command in a thread's bottle, its exit status folio's."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.bottle = f"{kept.repo(self.repo).key}-fix-it"

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
        refused = self.folio("sidecar", "exec", "fix-it", "--", "true")
        self.assertIn("has no sidecar", refused.stderr)


class CopyTest(FolioTestCase):
    """`folio cp`: into and out of a thread's bottle, as `cp -r` would."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        bottle = f"{kept.repo(self.repo).key}-fix-it"
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

    def test_a_sidecar_has_the_same_commands(self) -> None:
        refused = self.folio("sidecar", "cp", "fix-it:report.txt", ".")
        self.assertIn("has no sidecar", refused.stderr)
        self.assertIn("thread's sidecar: name one", self.folio("sidecar", "cp", "a", "b").stderr)

