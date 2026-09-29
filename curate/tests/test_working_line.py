"""The working line moving underneath a review."""

from .support import CurateTestCase


class WorkingLineMoves(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "f.txt")
        self.approved = self.repo.index_tree()

    def test_approvals_survive_a_commit_on_the_working_line(self) -> None:
        self.repo.curate("write")
        self.repo.write("g.txt", "new\n")
        self.repo.commit_all("more")
        self.repo.curate("review")
        self.assertEqual(self.repo.index_tree(), self.approved)
        self.assertEqual(self.repo.short_status(), ["M  f.txt", "?? g.txt"])

    def test_approvals_survive_a_rewritten_history(self) -> None:
        """Content-based approval is why rebasing and squashing cost nothing."""
        self.repo.curate("write")
        self.repo.git("commit", "-q", "--amend", "-m", "their work, reworded")
        self.repo.curate("review")
        self.assertEqual(self.repo.index_tree(), self.approved)
        self.assertEqual(self.repo.short_status(), ["M  f.txt"])

    def test_a_re_touched_file_comes_back_unapproved(self) -> None:
        self.repo.curate("write")
        self.repo.write("f.txt", "ONE\nTWO\nthree\n")
        self.repo.commit_all("changed it again")
        self.repo.curate("review")
        self.assertEqual(self.repo.short_status(), ["MM f.txt"])
        self.assertNotIn("+ONE", self.repo.git("diff"), "the approved line reappeared")

    def test_only_the_working_line_and_the_tree_move(self) -> None:
        clean_before = self.repo.git("rev-parse", self.repo.status_fields()["clean"])
        self.repo.curate("write")
        self.repo.write("g.txt", "new\n")
        self.repo.commit_all("more")
        self.repo.curate("review")
        self.assertEqual(self.repo.git("rev-parse", self.repo.status_fields()["clean"]),
                         clean_before)
