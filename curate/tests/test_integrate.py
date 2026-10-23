"""`curate integrate`: a finished review onto a branch, and the review gone."""

from .support import CurateTestCase

GENIE = {"GIT_AUTHOR_NAME": "genie", "GIT_AUTHOR_EMAIL": "genie@bottle.host",
         "GIT_AUTHOR_DATE": "2026-01-02T03:04:05+00:00"}
JANE = {"GIT_AUTHOR_NAME": "Jane", "GIT_AUTHOR_EMAIL": "jane@example.com"}


class Integrating(CurateTestCase):
    """A working line on main, written by someone else, under review."""

    def setUp(self) -> None:
        super().setUp()
        self.repo.write("a.txt", "a\n")
        self.base = self.repo.commit_all("base")
        self.repo.git("branch", "main")
        self.repo.git("switch", "-q", "-c", "feature")

    def commit(self, message: str, files: dict[str, str],
               by: dict[str, str] | None = None) -> str:
        for path, text in files.items():
            self.repo.write(path, text)
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", message, env=by or {})
        return self.repo.git("rev-parse", "HEAD")

    def review_all(self) -> None:
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "-A")

    def integrate(self, *args: str) -> str:
        result = self.repo.curate("integrate", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def refused(self, *args: str) -> str:
        before = self.repo.git("for-each-ref")
        result = self.repo.curate("integrate", *args)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertEqual(self.repo.git("for-each-ref"), before, "refused, yet changed")
        return result.stderr

    def author(self, rev: str) -> str:
        return self.repo.git("log", "-1", "--format=%an <%ae> %aI", rev)


class Adopting(Integrating):
    def setUp(self) -> None:
        super().setUp()
        self.first = self.commit("Add b", {"b.txt": "B\n"}, by=GENIE)
        self.tip = self.commit("Add c", {"c.txt": "C\n"}, by=GENIE)
        self.review_all()

    def test_adopted_commits_are_yours_with_their_dates_and_land_on_main(self) -> None:
        self.integrate("--into", "main", "--adopt", "genie")
        self.assertEqual(self.repo.log("main"), ["Add c", "Add b", "base"])
        date = self.repo.git("log", "-1", "--format=%aI", self.tip)
        self.assertEqual(self.author("main"), f"Test <test@example.com> {date}")
        self.assertEqual(self.repo.tree_of("main"), self.repo.tree_of(self.tip))

    def test_it_ends_on_the_destination_with_the_review_gone(self) -> None:
        out = self.integrate("--into", "main", "--adopt", "genie")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/main")
        self.assertEqual(self.repo.short_status(), [])
        self.assertEqual(self.repo.curate("list").stdout.strip(), "no reviews")
        self.assertEqual(self.repo.git("branch", "--list", "feature", "curate/*"), "")
        self.assertIn(f"'feature' (was {self.tip[:8]})", out)

    def test_not_adopting_keeps_the_commits_as_they_are(self) -> None:
        self.integrate("--into", "main", "--no-adopt", "genie")
        self.assertEqual(self.repo.git("rev-parse", "main"), self.tip)

    def test_a_new_branch_starts_from_the_base(self) -> None:
        self.integrate("--into", "mine", "--adopt", "genie@bottle.host")
        self.assertEqual(self.repo.log("mine"), ["Add c", "Add b", "base"])
        self.assertEqual(self.repo.git("rev-parse", "main"), self.base)

    def test_off_a_terminal_every_author_has_to_be_named(self) -> None:
        err = self.refused("--into", "main")
        self.assertIn("genie <genie@bottle.host> (2)", err)
        self.assertIn("--adopt <author>", err)

    def test_an_author_nobody_is_is_refused(self) -> None:
        self.assertIn("none of these commits is by 'bob'",
                      self.refused("--into", "main", "--adopt", "bob"))

    def test_a_branch_that_cannot_fast_forward_is_refused(self) -> None:
        self.repo.git("update-ref", "refs/heads/main", self.repo.git(
            "commit-tree", self.repo.tree_of(self.base), "-p", self.base, "-m", "moved"))
        self.assertIn("cannot be fast-forwarded",
                      self.refused("--into", "main", "--adopt", "genie"))

    def test_on_a_terminal_one_author_is_one_enter(self) -> None:
        out = self.on_a_terminal(["integrate"], ["\r", "\r", "mine", "\r"])
        self.assertIn("2 commit(s) by genie <genie@bottle.host> are not yours", out)
        self.assertIn(f"{self.tip[:8]}  Add c", out)        # which ones, to judge
        self.assertIn(f"{self.first[:8]}  Add b", out)
        self.assertIn("<new branch…>", out)
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "mine"), "Test")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/mine")

    def test_on_a_terminal_main_is_the_best_guess(self) -> None:
        out = self.on_a_terminal(["integrate"], ["\r", "j", "\r"])
        self.assertIn("best guess", out)
        self.assertEqual(self.repo.log("main"), ["Add c", "Add b", "base"])


class SeveralAuthors(Integrating):
    """Jane, then genie, then Jane again: only genie is adopted."""

    def setUp(self) -> None:
        super().setUp()
        self.jane1 = self.commit("Jane's first", {"b.txt": "B\n"}, by=JANE)
        self.genie = self.commit("genie's", {"c.txt": "C\n"}, by=GENIE)
        self.jane2 = self.commit("Jane's second", {"d.txt": "D\n"}, by=JANE)
        self.review_all()

    def test_every_author_has_to_be_decided_on(self) -> None:
        err = self.refused("--into", "main", "--adopt", "genie")
        self.assertIn("Not yet: Jane <jane@example.com>", err)

    def test_below_the_first_adopted_nothing_changes_and_above_it_authors_stay(self) -> None:
        self.integrate("--into", "main", "--adopt", "genie", "--no-adopt", "Jane")
        self.assertEqual(self.repo.git("rev-parse", "main~2"), self.jane1)
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "main~1"), "Test")
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "main"), "Jane")
        self.assertNotEqual(self.repo.git("rev-parse", "main"), self.jane2)

    def test_on_a_terminal_each_author_is_ticked_or_not(self) -> None:
        # The list is ordered newest first: Jane, then genie. Tick genie only,
        # and enter from the last row.
        out = self.on_a_terminal(["integrate"], ["j", " ", "\r", "/", "main", "\r"])
        self.assertIn(f"{self.genie[:8]}  genie's", out)
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "main~1"), "Test")
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "main"), "Jane")


class Unfinished(Integrating):
    def setUp(self) -> None:
        super().setUp()
        self.commit("Add b", {"b.txt": "B\n"})
        self.commit("Add c", {"c.txt": "C\n"})

    def test_anything_unreviewed_is_refused(self) -> None:
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "b.txt")
        self.assertIn("1 file(s) still unreviewed:\n  c.txt", self.refused("--into", "main"))

    def test_an_uncommitted_change_is_refused_even_when_approved(self) -> None:
        """Whoever writes the working line has not seen it."""
        self.review_all()
        self.repo.write("c.txt", "changed by hand\n")
        self.repo.git("add", "c.txt")
        err = self.refused("--into", "main")
        self.assertIn("not committed on 'feature'", err)
        self.assertIn("c.txt", err)

    def test_your_own_commits_need_no_question(self) -> None:
        self.review_all()
        tip = self.repo.git("rev-parse", "feature")
        self.integrate("--into", "main")
        self.assertEqual(self.repo.git("rev-parse", "main"), tip)


class Curated(Integrating):
    """A history curated on the clean line ships instead of the working one."""

    def setUp(self) -> None:
        super().setUp()
        self.commit("wip", {"b.txt": "B\n"}, by=GENIE)
        self.commit("more wip", {"c.txt": "C\n"}, by=GENIE)
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "b.txt")
        self.repo.git("commit", "-q", "-m", "Add b")

    def test_the_clean_line_ships_when_it_has_everything(self) -> None:
        self.repo.git("add", "c.txt")
        self.repo.git("commit", "-q", "-m", "Add c")
        self.integrate("--into", "main")
        self.assertEqual(self.repo.log("main"), ["Add c", "Add b", "base"])
        self.assertEqual(self.repo.git("branch", "--list", "feature", "curate/*"), "")

    def test_a_clean_line_with_only_some_of_it_is_refused(self) -> None:
        self.repo.git("add", "c.txt")                  # approved, not committed
        self.assertIn("not all of what you approved", self.refused("--into", "main"))


class Copied(Integrating):
    """feature-1 reviewed, then rebased into feature-2 and feature-3 with more
    on top each time, each review copied from the one before."""

    def setUp(self) -> None:
        super().setUp()
        self.repo.git("branch", "-m", "feature-1")
        self.commit("Add b", {"b.txt": "B\n"}, by=GENIE)
        self.review_all()
        self.repo.curate("write")
        self.next_version("feature-2", {"b.txt": "B\n", "c.txt": "C\n"}, "feature-1")

    def next_version(self, name: str, files: dict[str, str], copying: str) -> None:
        self.repo.git("switch", "-q", "-c", name, self.base)
        self.commit(f"squashed into {name}", files, by=GENIE)
        result = self.repo.curate("review", "--copy", copying)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.repo.git("add", "-A")

    def branches(self) -> str:
        return self.repo.git("branch", "--list", "feature*", "curate/*")

    def test_integrating_the_copy_ends_the_review_it_came_from(self) -> None:
        out = self.integrate("--into", "main", "--adopt", "genie")
        self.assertEqual(self.repo.curate("list").stdout.strip(), "no reviews")
        self.assertEqual(self.branches(), "")
        self.assertIn("so is the review of 'feature-1' it was copied from", out)

    def test_and_every_review_before_that(self) -> None:
        self.repo.curate("write")
        self.next_version("feature-3", {"b.txt": "B\n", "c.txt": "C\n", "d.txt": "D\n"},
                          "feature-2")
        out = self.integrate("--into", "main", "--adopt", "genie")
        self.assertEqual(self.repo.curate("list").stdout.strip(), "no reviews")
        self.assertEqual(self.branches(), "")
        self.assertIn("'feature-2' it was copied from", out)
        self.assertIn("'feature-1' it was copied from", out)

    def test_one_dropped_by_hand_already_is_simply_not_there(self) -> None:
        self.repo.curate("drop", "feature-1")
        self.integrate("--into", "main", "--adopt", "genie")
        self.assertEqual(self.repo.curate("list").stdout.strip(), "no reviews")

    def test_reviews_that_are_not_earlier_versions_are_left_alone(self) -> None:
        self.repo.curate("write")
        self.repo.git("switch", "-q", "-c", "unrelated", self.base)
        self.commit("Something else", {"e.txt": "E\n"})
        self.start(self.base)
        self.repo.git("switch", "-q", "feature-2")
        self.repo.curate("review")
        self.integrate("--into", "main", "--adopt", "genie")
        self.assertIn("unrelated", self.repo.curate("list").stdout)
        self.assertIn("curate/unrelated", self.branches())

    def test_dropping_a_copy_leaves_the_review_it_came_from(self) -> None:
        self.repo.curate("write")
        self.repo.curate("drop", "feature-2")
        self.assertIn("feature-1", self.repo.curate("list").stdout)


class CopiedOnTop(Integrating):
    """feature reviewed, then more pushed on a branch on top of it, the review
    transferred there, and that integrated back into feature."""

    def setUp(self) -> None:
        super().setUp()
        self.commit("Add b", {"b.txt": "B\n"}, by=GENIE)
        self.review_all()
        self.repo.curate("write")
        self.repo.git("switch", "-q", "-c", "on-top")
        self.commit("Add c", {"c.txt": "C\n"}, by=GENIE)
        result = self.repo.curate("review", "--copy", "feature")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.repo.git("add", "-A")

    def test_the_branch_it_came_from_is_kept_and_takes_it(self) -> None:
        result = self.repo.curate("integrate", "--into", "feature", "--adopt", "genie")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/feature")
        self.assertEqual(self.repo.git("log", "-1", "--format=%s", "feature"), "Add c")
        self.assertEqual(self.repo.git("branch", "--list", "on-top", "curate/*"), "")
        self.assertEqual(self.repo.curate("list").stdout.strip(), "no reviews")
        self.assertEqual(result.stderr, "")
