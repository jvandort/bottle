"""One review per branch, kept separately."""

from .support import CurateTestCase


class Sessions(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("work on wip")
        self.repo.git("branch", "second", self.base)

    def test_two_branches_keep_separate_reviews(self) -> None:
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.curate("write")
        self.repo.git("switch", "-q", "second")
        self.repo.write("g.txt", "other work\n")
        self.repo.commit_all("work on second")
        self.repo.curate("start", "--from", self.base)
        self.repo.curate("review")
        self.assertEqual(self.repo.short_status(), ["?? g.txt"])
        self.repo.curate("write")
        self.repo.git("switch", "-q", "wip")
        self.repo.curate("review")
        self.assertEqual(self.repo.index_tree(), approved, "the first review was lost")

    def test_list_shows_every_session_and_drop_removes_one(self) -> None:
        self.start(self.base)
        listed = self.repo.curate("list")
        self.assertIn("wip", listed.stdout)
        self.assertEqual(self.repo.curate("drop", "wip").returncode, 0)
        self.assertNotIn("wip", self.repo.curate("list").stdout)

    def test_a_branch_name_with_slashes_works(self) -> None:
        """A slash must survive into the session name and the review ref.

        Not `agent/foo` and `agent` together: refs/heads has the same
        restriction refs/curate does, so git refuses the second branch. The
        collision itself is pinned by GitAssumptions.
        """
        self.repo.git("switch", "-q", "-c", "agent/foo", self.base)
        self.repo.write("h.txt", "x\n")
        self.repo.commit_all("work")
        self.assertEqual(self.repo.curate("start", "--from", self.base).returncode, 0)
        self.repo.git("switch", "-q", "-c", "other/bar", self.base)
        self.repo.write("i.txt", "y\n")
        self.repo.commit_all("work")
        self.assertEqual(self.repo.curate("start", "--from", self.base).returncode, 0)

    def test_an_index_file_is_only_a_cache(self) -> None:
        """Deleting it must cost a re-stat, not the approvals."""
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.curate("write")
        for stale in (self.repo.path / ".git" / "curate").rglob("index.review"):
            stale.unlink()
        self.repo.curate("review")
        self.assertEqual(self.repo.index_tree(), approved)
