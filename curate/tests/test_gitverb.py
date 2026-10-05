"""`curate git <command>`: one command that borrows write mode and gives it back."""

import subprocess

from .support import CurateTestCase


class GitVerb(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.write("junk.txt", "debug\n")
        self.tip = self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")

    def test_it_runs_git_against_the_working_line(self) -> None:
        """In review mode HEAD is the clean line, so the log is the wrong one."""
        result = self.repo.curate("git", "log", "--format=%s")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["their work", "base"])

    def test_and_comes_back_to_review_mode(self) -> None:
        self.repo.curate("git", "log", "--oneline")
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "review")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), fields["clean"])

    def test_approvals_survive_the_round_trip(self) -> None:
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.curate("git", "status", "--short")
        self.assertEqual(self.repo.index_tree(), approved)

    def test_the_status_it_shows_is_the_working_line_s(self) -> None:
        """Not the review to-do list: that is what `git status` already is."""
        result = self.repo.curate("git", "status", "--short")
        self.assertEqual(result.stdout, "", result.stdout)

    def test_a_failing_command_still_comes_back(self) -> None:
        result = self.repo.curate("git", "no-such-command")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.repo.status_fields()["mode"], "review")

    def test_the_exit_code_is_git_s(self) -> None:
        self.assertEqual(self.repo.curate("git", "rev-parse", "--verify", "-q",
                                          "nope").returncode, 1)

    def test_it_does_not_touch_a_single_file(self) -> None:
        before = self.repo.mtimes()
        self.repo.curate("git", "log", "--oneline")
        self.assertEqual(self.repo.mtimes(), before)

    def test_in_write_mode_it_is_just_git(self) -> None:
        self.repo.curate("write")
        result = self.repo.curate("git", "log", "--format=%s")
        self.assertEqual(result.stdout.splitlines(), ["their work", "base"])
        self.assertEqual(self.repo.status_fields()["mode"], "write")

    def test_with_no_review_at_all_it_is_still_just_git(self) -> None:
        self.repo.curate("write")
        self.repo.curate("drop", "wip")
        result = self.repo.curate("git", "log", "--format=%s")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["their work", "base"])

    def test_it_says_nothing_of_its_own_when_all_goes_well(self) -> None:
        """It is a way of typing `git`, so it should read like one."""
        result = self.repo.curate("git", "log", "--format=%s", "-1")
        self.assertEqual(result.stderr, "")

    def test_no_command_is_refused(self) -> None:
        result = self.repo.curate("git")
        self.assertEqual(result.returncode, 1)
        self.assertIn("usage", result.stderr)
        self.assertEqual(self.repo.status_fields()["mode"], "review")


class WritingThroughIt(CurateTestCase):
    """The use it exists for: keeping up with a working line someone else writes."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")

    def test_a_commit_lands_on_the_working_line_and_the_review_returns(self) -> None:
        self.repo.curate("git", "commit", "--allow-empty", "-q", "-m", "from write mode")
        self.assertEqual(self.repo.log("wip")[0], "from write mode")
        self.assertEqual(self.repo.status_fields()["mode"], "review")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"),
                         self.repo.status_fields()["clean"])

    def test_pulling_a_new_commit_leaves_only_it_unreviewed(self) -> None:
        """Approvals are content, so what comes back unread is what is new."""
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "Everything")     # review finished
        self.assertEqual(self.repo.short_status(), [])
        self.repo.write("g.txt", "theirs\n")
        self.repo.curate("git", "add", "g.txt")
        self.repo.curate("git", "commit", "-q", "-m", "more of theirs")
        self.assertEqual(self.repo.status_fields()["mode"], "review")
        self.assertEqual(self.repo.short_status(), ["?? g.txt"])


class MovingHead(CurateTestCase):
    """A command that checks something else out has nowhere to come back to."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.git("branch", "elsewhere", self.base)
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "Everything")     # so git allows it

    def test_it_stays_in_write_mode_and_says_so(self) -> None:
        result = self.repo.curate("git", "switch", "-q", "elsewhere")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/elsewhere")
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "write")
        self.assertEqual(fields["consistent"], "yes")  # leaving the branch in write mode is ordinary
        self.assertIn("no\n        review to come back to", result.stderr)
        self.assertIn("git switch wip && curate review", result.stderr)

    def test_and_the_approved_set_is_still_there_afterwards(self) -> None:
        approved = self.repo.index_tree()
        self.repo.curate("git", "switch", "-q", "elsewhere")
        self.repo.git("switch", "-q", "wip")
        self.repo.curate("review")
        self.assertEqual(self.repo.index_tree(), approved)
