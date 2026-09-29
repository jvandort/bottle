"""The state-layout version, and what happens either side of it.

See STATE_VERSION in curate/state.py for when to bump it and what a bump has
to bring with it.
"""

from .support import CurateTestCase


class Versions(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.state = self.repo.path / ".git" / "curate"

    def test_the_tool_and_its_state_report_a_version(self) -> None:
        result = self.repo.curate("--version")
        self.assertEqual(result.returncode, 0)
        self.assertIn("curate", result.stdout)
        self.assertTrue((self.state / "version").exists())

    def test_state_from_a_newer_curate_is_refused_not_guessed_at(self) -> None:
        (self.state / "version").write_text("99\n")
        result = self.repo.curate("review")
        self.assertEqual(result.returncode, 1)
        self.assertIn("99", result.stderr)

    def test_state_from_an_older_curate_is_migrated(self) -> None:
        (self.state / "version").write_text("0\n")
        result = self.repo.curate("list")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("upgraded from version 0", result.stderr)
        self.assertEqual((self.state / "version").read_text().strip(),
                         self.repo.state_version())

    def test_and_migrating_costs_the_caches_but_not_the_approvals(self) -> None:
        """The whole basis of the migration: index files are derived."""
        self.repo.curate("review")
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.curate("write")
        (self.state / "version").write_text("0\n")
        self.assertEqual(self.repo.curate("list").returncode, 0)
        self.assertEqual(list(self.state.rglob("index.*")), [], "a cache survived")
        self.repo.curate("review")
        self.assertEqual(self.repo.index_tree(), approved, "approvals were lost")

    def test_a_missing_stamp_is_malformed_state(self) -> None:
        """No curate writes state without stamping it. Anything else is damage."""
        (self.state / "version").unlink()
        result = self.repo.curate("list")
        self.assertEqual(result.returncode, 1)
        self.assertIn("malformed", result.stderr)
        self.assertIn("is missing", result.stderr)

    def test_so_is_a_stamp_that_is_not_a_number(self) -> None:
        (self.state / "version").write_text("banana\n")
        result = self.repo.curate("list")
        self.assertEqual(result.returncode, 1)
        self.assertIn("malformed", result.stderr)

    def test_malformed_state_refuses_every_command_alike(self) -> None:
        """Including status: there is no leniency to be had anywhere."""
        (self.state / "version").unlink()
        for verb in ("status", "write", "review", "list", "fixup"):
            with self.subTest(verb=verb):
                self.assertEqual(self.repo.curate(verb).returncode, 1)

    def test_start_stamps_before_it_writes_anything_else(self) -> None:
        """So an interrupted start leaves state that is recognisable."""
        self.assertTrue((self.state / "version").exists())


class Problems(CurateTestCase):
    """What status reports that curate cannot put right by itself."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)

    def test_none_when_nothing_is_wrong(self) -> None:
        result = self.repo.curate("status")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("WRONG", result.stdout)
        self.assertEqual(self.repo.status_fields()["problems"], "0")

    def test_a_deleted_clean_line_is_reported(self) -> None:
        self.repo.git("update-ref", "-d", self.repo.status_fields()["clean"])
        result = self.repo.curate("status")
        self.assertIn("no longer exists", result.stdout)
        self.assertEqual(self.repo.status_fields()["problems"], "1")

    def test_a_deleted_clean_line_suggests_something_that_works(self) -> None:
        """Deleting a branch takes its reflog; HEAD's still has the commits."""
        clean = self.repo.status_fields()["clean"]
        tip = self.repo.git("rev-parse", clean)
        self.repo.git("update-ref", "-d", clean)
        said = self.repo.curate("status").stdout
        self.assertIn("git reflog", said)
        self.assertIn(f"git branch {clean[len('refs/heads/'):]} <oid>", said)
        self.repo.git("branch", clean[len("refs/heads/"):], tip)
        self.assertEqual(self.repo.status_fields()["problems"], "0")

    def test_a_deleted_working_line_is_reported(self) -> None:
        self.repo.curate("review")
        self.repo.git("update-ref", "-d", "refs/heads/wip")
        self.assertEqual(self.repo.status_fields()["problems"], "1")
