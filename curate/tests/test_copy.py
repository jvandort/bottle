"""Copying a review onto a branch an agent rebased into."""

from .support import CurateTestCase


class Copying(CurateTestCase):
    """A review of feature-1, part committed to the clean line, part approved."""

    def setUp(self) -> None:
        super().setUp()
        self.repo.write("a.txt", "a1\na2\na3\n")
        self.repo.write("g.txt", "g\n")
        self.base = self.repo.commit_all("base")
        self.repo.git("branch", "main")
        self.branch("feature-1", self.base, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "a.txt")
        self.repo.git("commit", "-q", "-m", "Change a")
        self.repo.git("add", "b.txt")                 # approved, not committed
        self.repo.curate("write")
        self.old = self.refs()

    def branch(self, name: str, start: str, files: dict[str, str]) -> str:
        self.repo.git("switch", "-q", "-c", name, start)
        for path, text in files.items():
            self.repo.write(path, text)
        return self.repo.commit_all(f"work on {name}")

    def upstream(self, files: dict[str, str]) -> str:
        """Move main on, as someone else would."""
        self.repo.git("switch", "-q", "main")
        for path, text in files.items():
            self.repo.write(path, text)
        return self.repo.commit_all("upstream")

    def refs(self) -> str:
        return self.repo.git("for-each-ref", "refs/heads/curate", "refs/curate")

    def old_refs(self) -> str:
        return "\n".join(line for line in self.refs().splitlines()
                         if "feature-1" in line)

    def copy(self, *args: str) -> str:
        result = self.repo.curate("start", "--copy", "feature-1", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def reviewing(self) -> list[str]:
        self.repo.curate("review")
        return self.repo.short_status()


class SameBase(Copying):
    def setUp(self) -> None:
        super().setUp()
        self.branch("feature-2", self.base,
                    {"a.txt": "A1\na2\na3\n", "b.txt": "B\n", "c.txt": "C\n"})

    def test_the_clean_line_and_the_approvals_come_across(self) -> None:
        self.copy()
        self.assertEqual(self.repo.git("rev-parse", "curate/feature-2"),
                         self.repo.git("rev-parse", "curate/feature-1"))
        self.assertEqual(self.reviewing(), ["A  b.txt", "?? c.txt"])

    def test_the_old_review_is_untouched(self) -> None:
        self.copy()
        self.reviewing()
        self.repo.git("add", "c.txt")
        self.repo.git("commit", "-q", "-m", "Add c")
        self.assertEqual(self.old_refs(), "\n".join(
            line for line in self.old.splitlines() if "feature-1" in line))
        self.assertIn("feature-1", self.repo.curate("list").stdout)

    def test_it_says_how_to_drop_either_one(self) -> None:
        out = self.copy()
        self.assertIn("curate drop feature-2` undoes the copy", out)
        self.assertIn("curate drop feature-1` once you are happy", out)

    def test_dropping_the_copy_deletes_its_clean_line_and_nothing_else(self) -> None:
        """Every commit on it is still on the old one, so nothing is lost."""
        self.copy()
        result = self.repo.curate("drop", "feature-2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("branch", "--list", "curate/feature-2"), "")
        self.assertEqual(self.refs(), self.old)

    def test_review_copy_begins_it_and_switches_in(self) -> None:
        result = self.repo.curate("review", "--copy", "feature-1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.status_fields()["mode"], "review")
        self.assertEqual(self.repo.short_status(), ["A  b.txt", "?? c.txt"])

    def test_review_copy_refuses_where_there_is_already_a_review(self) -> None:
        self.copy()
        result = self.repo.curate("review", "--copy", "feature-1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("already has one", result.stderr)

    def test_a_review_that_does_not_exist_is_refused(self) -> None:
        result = self.repo.curate("start", "--copy", "nonsense")
        self.assertEqual(result.returncode, 1)
        self.assertIn("no review of nonsense", result.stderr)
        self.assertEqual(self.repo.git("branch", "--list", "curate/feature-2"), "")

    def test_a_paused_fixup_is_finished_first(self) -> None:
        """Half a fixup is not a review anyone meant to copy."""
        session = self.repo.path / ".git" / "curate" / "sessions" / "curate%2Ffeature-1"
        (session / "fixup").mkdir()
        result = self.repo.curate("start", "--copy", "feature-1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("fixup", result.stderr)


class NewBase(Copying):
    def test_off_a_terminal_a_new_base_is_refused_rather_than_guessed(self) -> None:
        new = self.upstream({"g.txt": "G\n"})
        self.branch("feature-2", new, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        result = self.repo.curate("start", "--copy", "feature-1")
        self.assertEqual(result.returncode, 1)
        self.assertIn(new[:8], result.stderr)
        self.assertIn("--from", result.stderr)

    def test_the_clean_line_is_replayed_and_upstream_is_not_to_review(self) -> None:
        new = self.upstream({"g.txt": "G\n"})
        self.branch("feature-2", new, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        out = self.copy("--from", "main")
        self.assertIn("1 clean commit(s) replayed", out)
        self.assertEqual(self.repo.log("curate/feature-2"), ["Change a", "upstream", "base"])
        self.assertEqual(self.reviewing(), ["A  b.txt"])
        self.assertEqual(self.old_refs(), "\n".join(
            line for line in self.old.splitlines() if "feature-1" in line))

    def test_a_clean_commit_upstream_already_made_is_dropped(self) -> None:
        new = self.upstream({"a.txt": "A1\na2\na3\n"})
        self.branch("feature-2", new, {"b.txt": "B\n"})
        out = self.copy("--from", "main")
        self.assertIn("already in the new base: Change a", out)
        self.assertEqual(self.repo.git("rev-parse", "curate/feature-2"), new)
        self.assertEqual(self.reviewing(), ["A  b.txt"])

    def test_a_clean_commit_that_conflicts_ends_the_replay_without_stopping(self) -> None:
        """Its change was approved, but not of what the agent made of it."""
        new = self.upstream({"a.txt": "X1\na2\na3\n"})
        self.branch("feature-2", new, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        out = self.copy("--from", "main")
        self.assertIn("were not replayed", out)
        self.assertIn("Change a", out)
        self.assertIn("a.txt", out)                       # and so unreviewed
        self.assertEqual(self.repo.git("rev-parse", "curate/feature-2"), new)
        self.assertEqual(self.reviewing(), [" M a.txt", "A  b.txt"])

    def test_an_approval_that_conflicts_is_unreviewed_again(self) -> None:
        new = self.upstream({"b.txt": "upstream's b\n"})
        self.branch("feature-2", new, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        out = self.copy("--from", "main")
        self.assertIn("now unreviewed again", out)
        self.assertEqual(self.reviewing(), [" M b.txt"])

    def test_the_guess_follows_the_old_bases_branch_to_its_upstream(self) -> None:
        """Your main has not moved; the agent rebased onto origin/main."""
        self.repo.git("switch", "-q", "--detach", "main")
        self.repo.write("g.txt", "G\n")
        new = self.repo.commit_all("upstream")
        self.repo.git("update-ref", "refs/remotes/origin/main", new)
        self.repo.git("config", "branch.main.remote", "origin")
        self.repo.git("config", "branch.main.merge", "refs/heads/main")
        self.branch("feature-2", new, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        result = self.repo.curate("start", "--copy", "feature-1")
        self.assertIn(f"based on {new[:8]}", result.stderr)

    def test_on_a_terminal_the_list_names_the_old_base(self) -> None:
        new = self.upstream({"g.txt": "G\n"})
        self.branch("feature-2", new, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        out = self.on_a_terminal(["start", "--copy", "feature-1"], ["\r"])
        self.assertIn("Copying the review of 'feature-1'", out)
        self.assertIn(f"old base  {self.base[:8]}  (wip, main~1)", out)
        self.assertIn("best guess", out)
        self.assertEqual(self.repo.log("curate/feature-2"), ["Change a", "upstream", "base"])
