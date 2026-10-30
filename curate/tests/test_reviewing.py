"""`curate review <branch>`: pausing the review you are in to read another."""

from .support import CurateTestCase


class ReviewAnother(CurateTestCase):
    """Two branches of work off one base, `wip` under review and read, `other` not yet."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.git("branch", "other")
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.write("wip.txt", "wip's\n")
        self.repo.commit_all("wip's work")
        self.repo.git("switch", "-q", "other")
        self.repo.write("g.txt", "other's\n")
        self.other_tip = self.repo.commit_all("other's work")
        self.repo.git("switch", "-q", "wip")
        self.start(self.base)
        self.assertEqual(self.repo.curate("read").returncode, 0)
        self.repo.git("add", "wip.txt")                       # an approval, not yet committed

    def review(self, *args: str) -> str:
        result = self.repo.curate("review", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def refused(self, *args: str) -> str:
        result = self.repo.curate("review", *args)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        return result.stderr

    def where(self) -> tuple[str, str]:
        fields = self.repo.status_fields()
        return fields["mode"], fields["working"]

    def test_it_pauses_this_review_and_reads_the_other(self) -> None:
        out = self.review("other", "--from", self.base)
        self.assertEqual(("read", "refs/heads/other"), self.where())
        self.assertEqual("refs/heads/curate/other", self.repo.git("symbolic-ref", "HEAD"))
        self.assertEqual(["?? g.txt"], self.repo.short_status())  # other's work, unread
        self.assertIn("the review of 'wip' is paused", out)

    def test_coming_back_finds_the_approvals_where_they_were(self) -> None:
        self.review("other", "--from", self.base)
        self.repo.git("add", "g.txt")                         # an approval over there too
        self.review("wip")
        self.assertEqual(("read", "refs/heads/wip"), self.where())
        self.assertEqual(sorted(["A  wip.txt", " M f.txt"]), sorted(self.repo.short_status()))
        self.review("other")
        self.assertEqual(["A  g.txt"], self.repo.short_status())

    def test_an_edit_the_working_line_doesnt_have_refuses_and_nothing_moves(self) -> None:
        self.repo.write("f.txt", "ONE\ntwo\nmine\n")
        why = self.refused("other", "--from", self.base)
        self.assertIn("f.txt", why)
        self.assertIn("nothing has moved", why)
        self.assertEqual(("read", "refs/heads/wip"), self.where())
        self.assertEqual("ONE\ntwo\nmine\n", self.repo.read("f.txt"))
        self.assertIn("A  wip.txt", self.repo.short_status())  # the approval is still staged
        self.assertEqual("", self.repo.git("branch", "--list", "curate/other"))

    def test_a_new_file_refuses_too(self) -> None:
        self.repo.write("notes.txt", "scratch\n")
        self.assertIn("notes.txt", self.refused("other", "--from", self.base))
        self.assertEqual(("read", "refs/heads/wip"), self.where())

    def test_staged_in_write_mode_refuses(self) -> None:
        self.repo.curate("write")
        self.repo.write("staged.txt", "mine\n")             # git would carry this across
        self.repo.git("add", "staged.txt")
        self.assertIn("staged.txt", self.refused("other", "--from", self.base))
        self.assertEqual(("write", "refs/heads/wip"), self.where())

    def test_approvals_alone_are_not_pending_changes(self) -> None:
        # The staged wip.txt is an approval in read mode, and the review keeps it.
        self.review("other", "--from", self.base)
        self.assertEqual(("read", "refs/heads/other"), self.where())

    def test_a_paused_fixup_waits_with_its_review(self) -> None:
        session = self.repo.path / ".git" / "curate" / "sessions" / "curate%2Fwip"
        (session / "fixup").mkdir()
        (session / "fixup" / "marker").write_text("paused\n")
        self.review("other", "--from", self.base)
        self.assertEqual("paused\n", (session / "fixup" / "marker").read_text())

    def test_from_write_mode_too(self) -> None:
        self.repo.curate("write")
        self.review("other", "--from", self.base)
        self.assertEqual(("read", "refs/heads/other"), self.where())

    def test_no_review_there_and_no_base_off_a_terminal_refuses_before_moving(self) -> None:
        why = self.refused("other")
        self.assertIn("curate review other --from <base>", why)
        self.assertEqual(("read", "refs/heads/wip"), self.where())

    def test_a_clean_line_left_over_from_a_lost_review_is_named_as_such(self) -> None:
        self.repo.git("branch", "curate/other", self.base)
        why = self.refused("other", "--from", self.base)
        self.assertIn("curate/other already exists, but no review has it", why)
        self.assertIn("with nothing 'other' doesn't have", why)
        self.assertIn("git branch -D curate/other", why)
        self.assertEqual(("read", "refs/heads/wip"), self.where())

    def test_and_says_when_it_has_commits_of_its_own(self) -> None:
        self.repo.git("branch", "curate/other", "wip")
        self.assertIn("with commits of its own: `git log other..curate/other`",
                      self.refused("other", "--from", self.base))

    def test_from_is_only_for_beginning_one(self) -> None:
        self.review("other", "--from", self.base)
        self.assertIn("has a review already", self.refused("wip", "--from", self.base))

    def test_the_branch_you_are_on_is_just_read(self) -> None:
        self.repo.curate("write")
        self.review("wip")
        self.assertEqual(("read", "refs/heads/wip"), self.where())

    def test_a_branch_only_on_a_remote_is_made_and_tracks_it(self) -> None:
        self.repo.git("update-ref", "refs/remotes/bottle/t/genie/x", self.other_tip)
        self.repo.git("remote", "add", "bottle/t", "unused")
        self.review("genie/x", "--from", self.base)
        self.assertEqual(("read", "refs/heads/genie/x"), self.where())
        self.assertEqual("bottle/t", self.repo.git("config", "branch.genie/x.remote"))

    def test_a_clean_line_is_not_a_branch_to_review(self) -> None:
        self.assertIn("is a clean line", self.refused("curate/wip"))

    def test_no_such_branch(self) -> None:
        self.assertIn("no branch named nope", self.refused("nope"))

    def test_no_branch_off_a_terminal_says_what_to_type(self) -> None:
        why = self.refused()
        self.assertIn("curate review <branch>", why)
        self.assertIn("curate read", why)
