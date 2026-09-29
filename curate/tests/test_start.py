"""Starting a review, and undoing a start."""

from .support import CurateTestCase


class Start(CurateTestCase):
    def test_creates_a_clean_line_at_the_base(self) -> None:
        base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(base)
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "write")
        self.assertEqual(self.repo.git("rev-parse", fields["clean"]), base)

    def test_refuses_on_a_detached_head(self) -> None:
        base = self.make_base()
        self.repo.git("checkout", "-q", "--detach")
        self.assertEqual(self.repo.curate("start", "--from", base).returncode, 1)

    def test_refuses_when_a_branch_named_curate_exists(self) -> None:
        """Refs are paths: that branch rules out every curate/* clean line."""
        base = self.make_base()
        self.repo.git("branch", "curate", base)
        result = self.repo.curate("start", "--from", base)
        self.assertEqual(result.returncode, 1)
        self.assertIn("git branch -m curate", result.stderr)

    def test_and_naming_the_clean_line_yourself_does_not_excuse_it(self) -> None:
        """One rule, not one rule with an escape hatch nobody will remember."""
        base = self.make_base()
        self.repo.git("branch", "curate", base)
        result = self.repo.curate("start", "--from", base, "--clean", "wip-clean")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.repo.git("branch", "--list", "wip-clean"), "")

    def test_refuses_to_start_twice(self) -> None:
        base = self.make_base()
        self.start(base)
        self.assertEqual(self.repo.curate("start", "--from", base).returncode, 1)


class WrongBase(CurateTestCase):
    """The mistake you cannot see until review mode shows you the list.

    By then `start` has made a branch, so undoing it has to be one command that
    leaves the repository able to start again.
    """

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.wrong = self.repo.commit_all("their work")

    def branch_exists(self, name: str) -> bool:
        return bool(self.repo.git("rev-parse", "--verify", "-q",
                                  f"refs/heads/{name}", check=False))

    def test_start_says_how_to_undo_itself(self) -> None:
        """At the moment the base is chosen, which is when undoing is free."""
        result = self.repo.curate("start", "--from", self.wrong)
        self.assertIn("curate drop wip", result.stdout)

    def test_drop_deletes_a_clean_branch_nothing_was_approved_onto(self) -> None:
        self.start(self.wrong)
        self.assertEqual(self.repo.curate("drop", "wip").returncode, 0)
        self.assertFalse(self.branch_exists("curate/wip"))

    def test_and_so_the_same_review_can_be_started_again(self) -> None:
        """The whole point: leaving the branch behind made start refuse."""
        self.start(self.wrong)
        self.repo.curate("drop", "wip")
        result = self.repo.curate("start", "--from", self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_drop_keeps_a_clean_branch_with_work_on_it(self) -> None:
        """Once something is approved onto it, it is work, and work stays."""
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "f.txt")
        self.repo.git("commit", "-q", "-m", "Rename the thing")
        self.repo.curate("write")
        self.assertEqual(self.repo.curate("drop", "wip").returncode, 0)
        self.assertTrue(self.branch_exists("curate/wip"), "approved work was deleted")

    def test_dropping_the_review_you_are_in_leaves_review_mode_first(self) -> None:
        """Realising the base is wrong happens in review mode, not out of it."""
        self.start(self.wrong)
        self.repo.curate("review")
        result = self.repo.curate("drop", "wip")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")
        self.assertEqual(self.repo.short_status(), [], "left in review mode")
        self.assertIn("left review mode; HEAD is 'wip' again", result.stdout)
