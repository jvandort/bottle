"""`curate transfer`: the review you are in, onto the branch that replaced it."""

from .support import CurateTestCase


class Transfer(CurateTestCase):
    """feature-1 under review, tracking a remote the agent pushes to."""

    def setUp(self) -> None:
        super().setUp()
        remote = self.repo.path.parent / "agent.git"
        self.repo.git("init", "-q", "--bare", str(remote))
        self.repo.git("remote", "add", "agent", str(remote))
        self.repo.write("a.txt", "a1\na2\na3\n")
        self.base = self.repo.commit_all("base")
        self.repo.git("branch", "main")
        self.repo.git("switch", "-q", "-c", "feature-1")
        self.repo.write("a.txt", "A1\na2\na3\n")
        self.repo.write("b.txt", "B\n")
        self.repo.commit_all("agent work")
        self.repo.git("push", "-q", "-u", "agent", "feature-1")
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "a.txt")
        self.repo.git("commit", "-q", "-m", "Change a")
        self.repo.git("add", "b.txt")                 # approved, not committed

    def agent_pushes(self, name: str, parent: str, files: dict[str, str]) -> str:
        """The agent's rebase, arriving on the remote and not yet fetched."""
        tree = self.repo.git("rev-parse", f"{parent}^{{tree}}")
        idx = str(self.repo.path.parent / "agent-index")
        env = {"GIT_INDEX_FILE": idx}
        self.repo.git("read-tree", tree, env=env)
        for path, text in files.items():
            self.repo.git("update-index", "--add", "--cacheinfo",
                          f"100644,{self.repo.blob(text)},{path}", env=env)
        commit = self.repo.git("commit-tree", self.repo.git("write-tree", env=env),
                               "-p", parent, "-m", f"rebased into {name}")
        self.repo.git("push", "-q", "agent", f"{commit}:refs/heads/{name}")
        self.repo.git("update-ref", "-d", f"refs/remotes/agent/{name}")
        return commit

    def test_it_fetches_switches_copies_and_comes_back_in_review_mode(self) -> None:
        new = self.agent_pushes("feature-2", self.base,
                                {"a.txt": "A1\na2\na3\n", "b.txt": "B\n", "c.txt": "C\n"})
        result = self.repo.curate("transfer", "agent/feature-2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("rev-parse", "feature-2"), new)
        self.assertEqual(self.repo.git("rev-parse", "--abbrev-ref", "feature-2@{upstream}"),
                         "agent/feature-2")
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "review")
        self.assertEqual(fields["working"], "refs/heads/feature-2")
        self.assertEqual(self.repo.short_status(), ["A  b.txt", "?? c.txt"])
        self.assertIn("feature-1", self.repo.curate("list").stdout)

    def test_from_write_mode_it_comes_back_in_write_mode(self) -> None:
        self.repo.curate("write")
        self.agent_pushes("feature-2", self.base, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        result = self.repo.curate("transfer", "agent/feature-2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.status_fields()["mode"], "write")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/feature-2")

    def test_on_a_terminal_the_newest_branch_is_the_one_chosen(self) -> None:
        self.agent_pushes("feature-2", self.base, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        out = self.on_a_terminal(["transfer"], ["\r"])
        self.assertIn("Transfer the review of 'feature-1' to:", out)
        self.assertIn("<show local>", out)
        fields = self.repo.status_fields()
        self.assertEqual((fields["mode"], fields["working"]),
                         ("review", "refs/heads/feature-2"))

    def test_show_local_lists_local_branches(self) -> None:
        self.agent_pushes("feature-2", self.base, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        self.repo.git("branch", "mine", self.base)
        out = self.on_a_terminal(["transfer"], ["j", "\r", "q"])
        self.assertIn("mine", out)
        self.assertNotIn("curate/feature-1  ", out)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/curate/feature-1")

    def test_a_base_that_moved_is_asked_for_before_anything_changes(self) -> None:
        self.repo.curate("write")
        self.repo.git("switch", "-q", "main")
        self.repo.write("g.txt", "G\n")
        moved = self.repo.commit_all("upstream")
        self.repo.git("switch", "-q", "feature-1")
        self.repo.curate("review")
        self.agent_pushes("feature-2", moved, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        before = self.repo.git("for-each-ref", "refs/heads", "refs/curate")
        result = self.repo.curate("transfer", "agent/feature-2")
        self.assertEqual(result.returncode, 1)
        self.assertIn("--from", result.stderr)
        self.assertEqual(self.repo.git("branch", "--list", "feature-2"), "")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/curate/feature-1")
        self.assertEqual(self.repo.git("for-each-ref", "refs/heads", "refs/curate"),
                         before, "the fetch is all that may have changed")

    def test_a_branch_that_has_a_review_is_refused(self) -> None:
        self.agent_pushes("feature-2", self.base, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        self.assertEqual(self.repo.curate("transfer", "agent/feature-2").returncode, 0)
        self.repo.git("switch", "-q", "feature-1")
        result = self.repo.curate("transfer", "agent/feature-2")
        self.assertEqual(result.returncode, 1)
        self.assertIn("has a review already", result.stderr)

    def test_a_clean_line_is_refused_wherever_it_was_pushed(self) -> None:
        """Its local branch is a review's clean line, which no branch name says."""
        self.repo.git("push", "-q", "agent", "curate/feature-1")
        before = self.repo.git("for-each-ref", "refs/heads", "refs/curate")
        result = self.repo.curate("transfer", "agent/curate/feature-1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("is the clean line of a review", result.stderr)
        self.assertEqual(self.repo.git("for-each-ref", "refs/heads", "refs/curate"), before)

    def test_a_local_branch_that_diverged_is_refused(self) -> None:
        self.agent_pushes("feature-2", self.base, {"a.txt": "A1\na2\na3\n", "b.txt": "B\n"})
        self.repo.git("branch", "feature-2", self.base)
        self.repo.git("update-ref", "refs/heads/feature-2", self.repo.git(
            "commit-tree", self.repo.tree_of(self.base), "-p", self.base, "-m", "mine"))
        result = self.repo.curate("transfer", "agent/feature-2")
        self.assertEqual(result.returncode, 1)
        self.assertIn("diverged", result.stderr)

    def test_with_no_review_there_is_nothing_to_transfer(self) -> None:
        self.repo.curate("write")
        self.repo.git("switch", "-q", "main")
        result = self.repo.curate("transfer", "agent/feature-1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("no review here", result.stderr)
