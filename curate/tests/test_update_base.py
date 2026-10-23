"""`curate update-base`: a working line rebased in place, and its review moved
onto the new base with it."""

from .support import CurateTestCase


class UpdateBase(CurateTestCase):
    """feature reviewed -- part committed to the clean line, part approved --
    then rebased onto a main that moved on."""

    def setUp(self) -> None:
        super().setUp()
        self.repo.write("a.txt", "a1\na2\na3\n")
        self.repo.write("g.txt", "g\n")
        self.base = self.repo.commit_all("base")
        self.repo.git("branch", "main")
        self.repo.git("switch", "-q", "-c", "feature")
        self.repo.write("a.txt", "A1\na2\na3\n")
        self.repo.write("b.txt", "B\n")
        self.repo.commit_all("work on feature")
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "a.txt")
        self.repo.git("commit", "-q", "-m", "Change a")
        self.repo.git("add", "b.txt")                 # approved, not committed
        self.repo.curate("write")
        self.repo.git("switch", "-q", "main")
        self.repo.write("g.txt", "G upstream\n")
        self.main = self.repo.commit_all("upstream")
        self.repo.git("switch", "-q", "feature")
        self.repo.git("rebase", "-q", "main")

    def update_base(self, *args: str) -> str:
        result = self.repo.curate("update-base", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_what_upstream_did_is_not_left_to_review(self) -> None:
        self.repo.curate("review")
        self.assertIn(" M g.txt", self.repo.short_status())     # the reason for this
        self.update_base("main")
        self.assertEqual(self.repo.short_status(), ["A  b.txt"])
        self.assertEqual(self.repo.log("curate/feature"), ["Change a", "upstream", "base"])
        self.assertEqual(self.repo.status_fields()["base"], self.main)

    def test_it_comes_back_in_the_mode_it_left(self) -> None:
        self.update_base("main")
        self.assertEqual(self.repo.status_fields()["mode"], "write")
        self.repo.curate("review")
        self.repo.curate("update-base", self.base)
        self.assertEqual(self.repo.status_fields()["mode"], "review")

    def test_and_the_review_can_be_integrated(self) -> None:
        self.update_base("main")
        self.repo.curate("review")
        self.repo.git("commit", "-q", "-m", "Add b")
        result = self.repo.curate("integrate", "--into", "shipped")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.log("shipped"), ["Add b", "Change a", "upstream", "base"])

    def test_it_says_how_to_go_back(self) -> None:
        out = self.update_base("main")
        self.assertIn(f"`curate update-base {self.base[:8]}` goes back", out)
        self.assertIn("1 clean commit(s) replayed", out)

    def test_the_same_base_changes_nothing(self) -> None:
        before = self.repo.git("for-each-ref", "refs/heads", "refs/curate")
        self.assertIn("nothing to change", self.update_base(self.base))
        self.assertEqual(self.repo.git("for-each-ref", "refs/heads", "refs/curate"), before)

    def test_on_a_terminal_enter_takes_where_it_was_rebased_onto(self) -> None:
        out = self.on_a_terminal(["update-base"], ["\r"])
        self.assertIn("current base", out)
        self.assertEqual(self.repo.status_fields()["base"], self.main)

    def test_off_a_terminal_the_base_has_to_be_named(self) -> None:
        result = self.repo.curate("update-base")
        self.assertEqual(result.returncode, 1)
        self.assertIn("curate update-base <base>", result.stderr)

    def test_with_no_review_there_is_nothing_to_move(self) -> None:
        self.repo.git("switch", "-q", "main")
        result = self.repo.curate("update-base", "main")
        self.assertEqual(result.returncode, 1)
        self.assertIn("no review here", result.stderr)
