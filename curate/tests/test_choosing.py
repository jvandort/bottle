"""`curate start` and `curate review` with nothing to go on: which branch?

On a terminal they fetch, list every branch newest first with the one you are
on as the default, and only then ask where the clean line starts.
"""

from .support import CurateTestCase

LATER = "@4000000000 +0000"         # 2096: newer than anything committed today


class Choosing(CurateTestCase):
    """wip checked out, with an older main, and a remote to fetch from."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.git("branch", "main", self.base)
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        remote = self.repo.path.parent / "up.git"
        self.repo.git("init", "-q", "--bare", str(remote))
        self.repo.git("remote", "add", "up", str(remote))

    def newer_branch(self, name: str, subject: str) -> str:
        """A branch whose tip is newer than wip's, so it is listed first."""
        tip = self.repo.git("commit-tree", self.repo.tree_of("wip"), "-p", self.base,
                            "-m", subject, env={"GIT_COMMITTER_DATE": LATER})
        self.repo.git("branch", name, tip)
        return tip

    def test_the_current_branch_is_the_default(self) -> None:
        self.newer_branch("other", "elsewhere")
        out = self.on_a_terminal(["start"], ["\r", "\r"])
        self.assertIn("Review which branch?", out)
        self.assertIn("current", out)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_another_local_branch_is_switched_to_and_started(self) -> None:
        tip = self.newer_branch("other", "elsewhere")
        self.on_a_terminal(["start"], ["g", "\r", "\r"])
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/other")
        self.assertEqual(self.repo.status_fields()["working"], "refs/heads/other")
        self.assertEqual(self.repo.git("rev-parse", "other"), tip)

    def test_review_asks_too_and_ends_up_reviewing_the_choice(self) -> None:
        self.newer_branch("other", "elsewhere")
        self.on_a_terminal(["review"], ["g", "\r", "\r"])
        fields = self.repo.status_fields()
        self.assertEqual((fields["mode"], fields["working"]),
                         ("review", "refs/heads/other"))

    def test_a_branch_only_on_the_remote_is_fetched_and_made_local(self) -> None:
        tip = self.newer_branch("pushed", "from somebody else")
        self.repo.git("push", "-q", "up", "pushed")
        self.repo.git("branch", "-D", "pushed")
        self.repo.git("update-ref", "-d", "refs/remotes/up/pushed")   # not fetched yet
        out = self.on_a_terminal(["start"], ["g", "\r", "\r"])
        self.assertIn("up/pushed", out)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/pushed")
        self.assertEqual(self.repo.git("rev-parse", "pushed"), tip)
        self.assertEqual(self.repo.git("rev-parse", "--abbrev-ref", "pushed@{upstream}"),
                         "up/pushed")

    def test_a_remote_branch_you_have_locally_is_listed_once(self) -> None:
        self.repo.git("push", "-q", "up", "wip")
        out = self.on_a_terminal(["start"], ["q"])
        self.assertNotIn("up/wip", out)

    def test_cancelling_the_base_leaves_you_where_you_were(self) -> None:
        self.newer_branch("pushed", "from somebody else")
        self.repo.git("push", "-q", "up", "pushed")
        self.repo.git("branch", "-D", "pushed")
        out = self.on_a_terminal(["start"], ["g", "\r", "q"])
        self.assertIn("cancelled", out)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")
        self.assertEqual(self.repo.git("branch", "--list", "pushed"), "")
        self.assertFalse((self.repo.path / ".git" / "curate" / "sessions").exists())

    def test_a_branch_with_a_review_is_gone_back_to_rather_than_started(self) -> None:
        self.newer_branch("other", "elsewhere")
        self.repo.git("switch", "-q", "other")
        self.start(self.base)
        self.repo.curate("write")
        self.repo.git("switch", "-q", "wip")
        out = self.on_a_terminal(["review"], ["g", "\r"])  # other is newest
        self.assertIn("has a review", out)
        fields = self.repo.status_fields()
        self.assertEqual((fields["mode"], fields["working"]),
                         ("review", "refs/heads/other"))

    def test_with_arguments_it_does_not_ask(self) -> None:
        self.newer_branch("other", "elsewhere")
        out = self.on_a_terminal(["start", "--from", self.base], [])
        self.assertNotIn("Review which branch?", out)
        self.assertEqual(self.repo.status_fields()["working"], "refs/heads/wip")

    def test_a_remote_named_inside_another_is_told_apart(self) -> None:
        """`up` and `up/x` both remotes, as `bottle` and `bottle/rebase` are:
        up/x/feature is up/x's feature, and git will not say so by itself."""
        nested = self.repo.path.parent / "x.git"
        self.repo.git("init", "-q", "--bare", str(nested))
        self.repo.git("remote", "add", "up/x", str(nested))
        self.newer_branch("feature", "theirs")
        self.repo.git("push", "-q", "up/x", "feature")
        self.repo.git("branch", "-D", "feature")
        self.on_a_terminal(["start"], ["g", "\r", "\r"])
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/feature")
        self.assertEqual(self.repo.git("rev-parse", "--symbolic-full-name", "feature@{upstream}"),
                         "refs/remotes/up/x/feature")

    def test_listing_asks_for_the_remotes_once_not_once_a_branch(self) -> None:
        """1,600 remote branches once cost 1,600 `git remote`s: half a minute
        before the list appeared."""
        for i in range(30):
            self.repo.git("update-ref", f"refs/remotes/up/b{i}", self.base)
        self.on_a_terminal(["start"], ["\r", "\r"], env={"CURATE_SLOW": "0"})
        log = (self.repo.path / ".git" / "curate" / "log").read_text()
        asked = [line for line in log.splitlines() if line.endswith(" git remote")]
        self.assertLessEqual(len(asked), 2, log)

    def test_each_tag_is_on_its_own_row(self) -> None:
        self.newer_branch("other", "elsewhere")
        self.repo.git("switch", "-q", "other")
        self.start(self.base)
        self.repo.curate("write")
        self.repo.git("switch", "-q", "wip")
        out = self.on_a_terminal(["start"], ["q"])
        self.assertRegex(out, r"other +has a review")
        self.assertRegex(out, r"wip +current")
        self.assertNotRegex(out, r"main +(current|has a review)")
