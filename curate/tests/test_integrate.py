"""`curate integrate`: a finished review onto a branch, and the review gone."""

import subprocess

from .support import CurateTestCase
from .test_signing import fake_gpg, signed

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
        self.repo.curate("read")
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

    def test_adopted_commits_are_signed_when_commits_are(self) -> None:
        fake_gpg(self.repo)
        self.repo.git("config", "commit.gpgsign", "true")
        self.integrate("--into", "main", "--adopt", "genie")
        self.assertTrue(signed(self.repo, "main"))
        self.assertTrue(signed(self.repo, "main~1"))

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

    def test_another_reviews_clean_line_is_refused_whatever_it_is_called(self) -> None:
        self.repo.curate("write")
        self.repo.git("switch", "-q", "-c", "other", self.base)
        self.repo.curate("start", "--from", self.base, "--clean", "other-clean")
        self.repo.git("switch", "-q", "feature")
        self.repo.curate("read")
        self.assertIn("other-clean belongs to a review",
                      self.refused("--into", "other-clean", "--adopt", "genie"))

    def test_a_branch_without_the_base_is_refused(self) -> None:
        self.repo.git("update-ref", "refs/heads/elsewhere", self.repo.git(
            "commit-tree", self.repo.tree_of(self.base), "-m", "unrelated"))
        self.assertIn("cannot be fast-forwarded to this, nor replayed onto",
                      self.refused("--into", "elsewhere", "--adopt", "genie"))

    def test_on_a_terminal_one_author_is_one_enter(self) -> None:
        out = self.on_a_terminal(["integrate"], ["\r", "\r", "mine", "\r"])
        self.assertIn("2 commit(s) by genie <genie@bottle.host> are not yours", out)
        self.assertIn(f"{self.tip[:8]}  Add c", out)        # which ones, to judge
        self.assertIn(f"{self.first[:8]}  Add b", out)
        self.assertIn("<new branch…>", out)
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "mine"), "Test")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/mine")

    def test_on_a_terminal_only_branches_it_fast_forwards_or_replays_onto_are_offered(self) -> None:
        self.repo.git("branch", "behind", self.base)
        self.repo.git("branch", "moved", self.repo.git(
            "commit-tree", self.repo.tree_of(self.base), "-p", self.base, "-m", "moved"))
        self.repo.git("branch", "unrelated", self.repo.git(
            "commit-tree", self.repo.tree_of(self.base), "-m", "unrelated"))
        out = self.on_a_terminal(["integrate"], ["\r", "q"])
        self.assertIn("behind", out)
        self.assertIn("replays onto it", out)
        self.assertNotIn("unrelated", out)

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
        out = self.integrate("--into", "main", "--adopt", "genie", "--no-adopt", "Jane")
        self.assertIn("integrated 3 commit(s)", out)
        self.assertIn("adopted 1 of them as yours", out)
        self.assertEqual(self.repo.git("rev-parse", "main~2"), self.jane1)
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "main~1"), "Test")
        self.assertEqual(self.repo.git("log", "-1", "--format=%an", "main"), "Jane")
        self.assertNotEqual(self.repo.git("rev-parse", "main"), self.jane2)

    def test_on_a_terminal_each_author_is_ticked_or_not(self) -> None:
        # The list is ordered newest first: Jane, then genie. Tick genie only,
        # and enter from the last row.
        out = self.on_a_terminal(["integrate"], ["j", " ", "\r", "/", "main", "\r", "\r"])
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
        self.repo.curate("read")
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
        self.repo.curate("read")
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


class Refusals(Integrating):
    def test_a_working_line_still_at_the_base_has_nothing_to_integrate(self) -> None:
        self.review_all()
        self.assertIn("nothing to integrate", self.refused("--into", "main"))

    def test_a_flag_with_no_value_is_refused(self) -> None:
        self.review_all()
        self.assertIn("--into needs a value", self.refused("--into"))

    def test_a_branch_checked_out_in_another_worktree_is_refused(self) -> None:
        """Moving it would leave that worktree's index and files behind."""
        self.commit("Add b", {"b.txt": "B\n"})
        self.review_all()
        self.repo.git("worktree", "add", "-q", str(self.repo.path.parent / "other"), "main")
        self.assertIn("checked out in another worktree", self.refused("--into", "main"))

    def test_with_no_review_there_is_nothing_to_integrate(self) -> None:
        self.assertIn("no review here to integrate", self.refused())

    def test_an_author_cannot_be_both_adopted_and_kept(self) -> None:
        self.commit("Add b", {"b.txt": "B\n"}, by=GENIE)
        self.review_all()
        self.assertIn("cannot be both adopted and not",
                      self.refused("--into", "main", "--adopt", "genie", "--no-adopt", "genie"))

    def test_a_name_two_authors_share_has_to_be_an_email(self) -> None:
        self.commit("Add b", {"b.txt": "B\n"}, by=GENIE)
        self.commit("Add c", {"c.txt": "C\n"}, by={**GENIE, "GIT_AUTHOR_EMAIL": "other@x"})
        self.review_all()
        said = self.refused("--into", "main", "--adopt", "genie")
        self.assertIn("could be any of", said)
        self.assertIn("use the email", said)


class Missing(Integrating):
    def setUp(self) -> None:
        super().setUp()
        self.commit("Add b", {"b.txt": "B\n"})
        self.review_all()
        self.repo.curate("write")

    def test_a_clean_line_deleted_by_hand_is_refused(self) -> None:
        self.repo.git("branch", "-D", "curate/feature")
        self.assertIn("a line of this review is missing", self.refused("--into", "main"))


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
        result = self.repo.curate("read", "--copy", copying)
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
        self.repo.curate("read")
        self.integrate("--into", "main", "--adopt", "genie")
        self.assertIn("unrelated", self.repo.curate("list").stdout)
        self.assertIn("curate/unrelated", self.branches())

    def test_a_review_started_since_on_the_same_branch_is_not_an_earlier_one(self) -> None:
        """Dropped by hand, then reviewed again: same branch, same clean line
        name, and nothing to do with the copy."""
        self.repo.curate("write")
        self.repo.curate("drop", "feature-1")
        self.repo.git("switch", "-q", "feature-1")
        self.commit("New work", {"n.txt": "N\n"}, by=GENIE)
        self.start(self.base)
        self.repo.git("switch", "-q", "feature-2")
        self.repo.curate("read")
        self.integrate("--into", "main", "--adopt", "genie")
        self.assertIn("feature-1", self.repo.curate("list").stdout)
        self.assertEqual(self.repo.git("log", "-1", "--format=%s", "feature-1"), "New work")

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
        result = self.repo.curate("read", "--copy", "feature")
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


class Replaying(Integrating):
    """genie's two commits reviewed on feature; main moved on since, by a commit of its own."""

    def setUp(self) -> None:
        super().setUp()
        self.first = self.commit("Add b", {"b.txt": "B\n"}, by=GENIE)
        self.tip = self.commit("Add c", {"c.txt": "C\n"}, by=GENIE)
        self.review_all()
        self.moved = self.move_main({"x.txt": "X\n"})

    def move_main(self, files: dict[str, str], message: str = "Elsewhere") -> str:
        """A commit on main, made without touching the files or HEAD of the review."""
        index = str(self.repo.path / ".git" / "other-index")
        env = {"GIT_INDEX_FILE": index}
        self.repo.git("read-tree", "main", env=env)
        for path, text in files.items():
            blob = self.repo.blob(text)
            self.repo.git("update-index", "--add", "--cacheinfo", f"100644,{blob},{path}", env=env)
        commit = self.repo.git("commit-tree", self.repo.git("write-tree", env=env), "-p", "main", "-m", message)
        self.repo.git("update-ref", "refs/heads/main", commit)
        return commit

    def test_off_a_terminal_it_takes_replay_to_replay(self) -> None:
        err = self.refused("--into", "main", "--adopt", "genie")
        self.assertIn("main' has moved on by 1 commit(s) since this review's base", err)
        self.assertIn("--replay", err)

    def test_replayed_onto_main_with_what_it_had_and_adopted(self) -> None:
        out = self.integrate("--into", "main", "--adopt", "genie", "--replay")
        self.assertEqual(self.repo.log("main"), ["Add c", "Add b", "Elsewhere", "base"])
        self.assertEqual(self.repo.git("rev-parse", "main~2"), self.moved)
        date = self.repo.git("log", "-1", "--format=%aI", self.tip)
        self.assertEqual(self.author("main"), f"Test <test@example.com> {date}")
        self.assertEqual(sorted(self.repo.git("ls-tree", "--name-only", "main").split()),
                         ["a.txt", "b.txt", "c.txt", "x.txt"])
        self.assertIn("replayed onto it", out)
        self.assertIn("untested", out)

    def test_your_files_become_the_destinations(self) -> None:
        self.integrate("--into", "main", "--no-adopt", "genie", "--replay")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/main")
        self.assertEqual(self.repo.read("x.txt"), "X\n")
        self.assertEqual(self.repo.short_status(), [])
        self.assertEqual(self.author("main"), self.author(self.tip))
        self.assertEqual(self.repo.curate("list").stdout.strip(), "no reviews")

    def test_a_commit_main_has_already_is_dropped(self) -> None:
        """As when one branch's commits were integrated, rewritten, before another that has them."""
        self.move_main({"b.txt": "B\n"}, "Add b, adopted")
        out = self.integrate("--into", "main", "--adopt", "genie", "--replay")
        self.assertEqual(self.repo.log("main"), ["Add c", "Add b, adopted", "Elsewhere", "base"])
        self.assertIn("integrated 1 commit(s)", out)
        self.assertIn(f"1 more had nothing left to do there, so were dropped: {self.first[:8]}", out)

    def test_a_conflict_changes_nothing(self) -> None:
        self.move_main({"c.txt": "not C\n"})
        err = self.refused("--into", "main", "--adopt", "genie", "--replay")
        self.assertIn(f"replaying {self.tip[:8]} conflicts, in:\n  c.txt", err)
        self.assertIn("feature", self.repo.curate("list").stdout)
        self.assertEqual(self.repo.read("c.txt"), "C\n")

    def test_on_a_terminal_it_asks_first(self) -> None:
        out = self.on_a_terminal(["integrate", "--into", "main", "--adopt", "genie"], ["\r"])
        self.assertIn("Replay the commits onto it", out)
        self.assertEqual(self.repo.log("main"), ["Add c", "Add b", "Elsewhere", "base"])

    def test_on_a_terminal_picking_it_from_the_list_is_the_say_so(self) -> None:
        out = self.on_a_terminal(["integrate", "--adopt", "genie"], ["j", "j", "\r"])  # past wip, at the base
        self.assertIn("replays onto it, best guess", out)
        self.assertNotIn("Replay the commits onto it", out)
        self.assertEqual(self.repo.log("main"), ["Add c", "Add b", "Elsewhere", "base"])


class DeletingUpstream(Copied):
    """feature-1's review copied into feature-2's, each tracking a branch of a remote."""

    def setUp(self) -> None:
        super().setUp()
        self.remote = self.repo.path.parent / "remote.git"
        subprocess.run(["git", "init", "-q", "--bare", str(self.remote)], check=True)
        self.repo.git("remote", "add", "up", str(self.remote))
        for name in ("feature-1", "feature-2"):
            self.repo.git("push", "-q", "-u", "up", f"{name}:genie/{name}")

    def upstream(self) -> str:
        return subprocess.run(["git", "-C", str(self.remote), "branch", "--format=%(refname:short)"],
                              capture_output=True, text=True, check=True).stdout.strip()

    def test_each_ended_reviews_upstream_is_deleted_where_the_remote_says_to(self) -> None:
        self.repo.git("config", "remote.up.curateDelete", "true")
        out = self.integrate("--into", "main", "--adopt", "genie")
        self.assertEqual(self.upstream(), "")
        self.assertIn("Deleted genie/feature-2 from up.", out)
        self.assertIn("Deleted genie/feature-1 from up.", out)

    def test_not_where_it_doesnt(self) -> None:
        out = self.integrate("--into", "main", "--adopt", "genie")
        self.assertEqual(self.upstream(), "genie/feature-1\ngenie/feature-2")
        self.assertNotIn("Deleted", out)

    def test_a_refused_delete_is_said_and_the_integrate_stands(self) -> None:
        self.repo.git("config", "remote.up.curateDelete", "true")
        subprocess.run(["git", "-C", str(self.remote), "config", "receive.denyDeletes", "true"], check=True)
        out = self.integrate("--into", "main", "--adopt", "genie")
        self.assertIn("Kept genie/feature-2 on up: it refused the delete. The integrate stands.", out)
        self.assertEqual(self.repo.curate("list").stdout.strip(), "no reviews")
