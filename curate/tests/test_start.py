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

    def test_status_says_where_the_clean_line_starts(self) -> None:
        base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(base)
        self.assertEqual(self.repo.status_fields()["base"], base)
        self.assertRegex(self.repo.curate("status").stdout,
                         rf"(?m)^base: +{base[:8]} .*base$")

    def test_refuses_on_a_detached_head(self) -> None:
        base = self.make_base()
        self.repo.git("checkout", "-q", "--detach")
        result = self.repo.curate("start", "--from", base)
        self.assertEqual(result.returncode, 1)
        self.assertIn("HEAD is detached", result.stderr)

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

    def test_off_a_terminal_the_base_is_where_the_branch_left_its_upstream(self) -> None:
        base = self.make_base()
        self.repo.git("branch", "main")
        self.repo.git("branch", "--set-upstream-to", "main")
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.repo.git("update-ref", "refs/heads/main", self.repo.git(
            "commit-tree", self.repo.tree_of(base), "-p", base, "-m", "upstream moved on"))
        result = self.repo.curate("start")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), base)

    def test_and_with_no_upstream_it_asks_for_from(self) -> None:
        self.make_base()
        result = self.repo.curate("start")
        self.assertEqual(result.returncode, 1)
        self.assertIn("pass --from <base>", result.stderr)
        self.assertEqual(self.repo.git("branch", "--list", "curate/wip"), "")

    def test_a_base_that_does_not_resolve_is_refused(self) -> None:
        self.make_base()
        result = self.repo.curate("start", "--from", "no-such-base")
        self.assertEqual(result.returncode, 1)
        self.assertIn("cannot resolve no-such-base", result.stderr)

    def test_starts_in_the_middle_of_a_conflicted_merge(self) -> None:
        """An unmerged live index cannot seed the review's; it is built
        without the stat cache instead."""
        base = self.make_base()
        self.repo.git("switch", "-q", "-c", "other")
        self.repo.write("f.txt", "OTHER\ntwo\nthree\n")
        self.repo.commit_all("other")
        self.repo.git("switch", "-q", "wip")
        self.repo.write("f.txt", "WIP\ntwo\nthree\n")
        self.repo.commit_all("wip")
        self.repo.git("merge", "-q", "other", check=False)
        self.assertIn("UU f.txt", self.repo.short_status())
        self.start(base)
        index = self.repo.path / ".git" / "curate" / "sessions" / "curate%2Fwip" / "index.review"
        self.assertEqual(self.repo.index_tree(str(index)), self.repo.tree_of(base))

    def test_refuses_from_review_mode(self) -> None:
        base = self.make_base()
        self.start(base)
        self.repo.curate("review")
        result = self.repo.curate("start", "--from", base, "--clean", "another")
        self.assertEqual(result.returncode, 1)
        self.assertIn("finish the current review first", result.stderr)

    def test_a_clean_line_named_after_an_existing_branch_is_refused(self) -> None:
        base = self.make_base()
        self.repo.git("branch", "taken", base)
        result = self.repo.curate("start", "--from", base, "--clean", "taken")
        self.assertEqual(result.returncode, 1)
        self.assertIn("branch taken already exists", result.stderr)


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

    def test_the_reviews_own_approvals_do_not_count_as_another_branch(self) -> None:
        """The approved set is a commit on top of the clean line, so it reaches
        every commit there. It goes with the review, so it cannot vouch for them."""
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "f.txt")
        self.repo.git("commit", "-q", "-m", "Rename the thing")
        self.repo.write("f.txt", "ONE\nTWO\nthree\n")
        self.repo.git("add", "f.txt")                  # approved, not committed
        self.repo.curate("write")
        self.repo.curate("drop", "wip")
        self.assertTrue(self.branch_exists("curate/wip"), "approved work was deleted")

    def test_drop_deletes_a_clean_branch_whose_commits_another_branch_has(self) -> None:
        """Pushed, or copied into another review: deleting it loses nothing."""
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "f.txt")
        self.repo.git("commit", "-q", "-m", "Rename the thing")
        self.repo.curate("write")
        self.repo.git("branch", "elsewhere", "curate/wip")
        result = self.repo.curate("drop", "wip")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.branch_exists("curate/wip"))
        self.assertIn("on another branch too", result.stdout)

    def test_dropping_the_review_you_are_in_leaves_review_mode_first(self) -> None:
        """Realising the base is wrong happens in review mode, not out of it."""
        self.start(self.wrong)
        self.repo.curate("review")
        result = self.repo.curate("drop", "wip")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")
        self.assertEqual(self.repo.short_status(), [], "left in review mode")
        self.assertIn("left review mode; HEAD is 'wip' again", result.stdout)

    def test_drop_with_no_branch_drops_the_review_you_are_in(self) -> None:
        """You are standing on it; naming it again is busywork."""
        self.start(self.wrong)
        self.repo.curate("review")
        result = self.repo.curate("drop")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.branch_exists("curate/wip"))
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")

    def test_drop_leaves_a_clean_line_checked_out_elsewhere_and_its_review(self) -> None:
        self.start(self.wrong)
        self.repo.git("worktree", "add", "-q", str(self.repo.path.parent / "other"),
                      "curate/wip")
        result = self.repo.curate("drop", "wip")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(self.branch_exists("curate/wip"))
        self.assertIn("wip", self.repo.curate("list").stdout)

    def test_drop_says_so_when_the_clean_line_was_already_gone(self) -> None:
        self.start(self.wrong)
        self.repo.git("branch", "-D", "curate/wip")
        result = self.repo.curate("drop", "wip")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("was already gone", result.stdout)
        self.assertNotIn("deleted", result.stdout)

    def test_drop_with_no_branch_works_from_write_mode_too(self) -> None:
        self.start(self.wrong)
        result = self.repo.curate("drop")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.branch_exists("curate/wip"))
        self.assertEqual(self.repo.status_fields()["mode"], "none")

    def test_drop_with_no_branch_follows_head_moved_by_git(self) -> None:
        """HEAD moved behind curate's back still names the review to drop."""
        self.start(self.wrong)
        self.repo.curate("review")
        self.repo.curate("write")
        self.repo.git("switch", "-q", "curate/wip")
        result = self.repo.curate("drop")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("branch", "--list", "curate/wip"), "")

    def test_drop_with_no_branch_refuses_off_any_review(self) -> None:
        self.start(self.wrong)
        self.repo.git("switch", "-q", "-c", "elsewhere")
        result = self.repo.curate("drop")
        self.assertEqual(result.returncode, 1)
        self.assertIn("curate drop <branch>", result.stderr)
        self.assertTrue(self.branch_exists("curate/wip"), "dropped a review HEAD is not in")
