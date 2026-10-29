from folio import repos
from tests.support import FolioTestCase, git


class RenameTest(FolioTestCase):
    """fix-it, with a branch the agent pushed, fetched and checked out by you; then renamed to sort-it."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.key = repos.current(self.repo).key
        tree = git("-C", str(self.repo), "rev-parse", "HEAD^{tree}")
        self.pushed = git("-C", str(self.repo), "commit-tree", tree, "-p", "HEAD", "-m", "agent work")
        git("-C", str(self.repo), "update-ref", "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1", self.pushed)
        git("-C", str(self.repo), "update-ref", "refs/namespaces/fix-it-in/refs/heads/handed", self.pushed)
        git("-C", str(self.repo), "fetch", "-q", "bottle/fix-it")
        git("-C", str(self.repo), "switch", "-q", "genie/fix-it-1")

    def refs(self, prefix: str) -> list[str]:
        return git("-C", str(self.repo), "for-each-ref", "--format=%(refname)", prefix).splitlines()

    def test_a_thread_with_a_bottle_is_paused_first(self) -> None:
        result = self.folio("rename", "fix-it", "sort-it")
        self.assertNotEqual(0, result.returncode)
        self.assertIn("pause it first", result.stderr)
        self.assertIn("folio pause fix-it", result.stderr)
        self.assertEqual(["refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1"], self.refs("refs/namespaces/fix-it-out/"))
        self.assertIn("fix-it", self.ok("list"))

    def test_everything_moves_and_the_branches_keep_their_names(self) -> None:
        self.ok("pause", "fix-it")
        out = self.ok("rename", "fix-it", "sort-it")
        self.assertIn("Renamed thread fix-it to sort-it", out)
        self.assertEqual(["refs/namespaces/sort-it-out/refs/heads/genie/fix-it-1"], self.refs("refs/namespaces/sort-it-out/"))
        self.assertEqual(["refs/namespaces/sort-it-in/refs/heads/handed"], self.refs("refs/namespaces/sort-it-in/"))
        self.assertEqual([], self.refs("refs/namespaces/fix-it-out/") + self.refs("refs/namespaces/fix-it-in/"))
        # The remote, and your branch that tracked it, move to the new name.
        self.assertEqual(["refs/remotes/bottle/sort-it/genie/fix-it-1"], self.refs("refs/remotes/bottle/"))
        self.assertEqual("bottle/sort-it/genie/fix-it-1",
                         git("-C", str(self.repo), "rev-parse", "--abbrev-ref", "genie/fix-it-1@{upstream}"))
        self.assertEqual("git --namespace=sort-it-out upload-pack",
                         git("-C", str(self.repo), "config", "remote.bottle/sort-it.uploadpack"))
        git("-C", str(self.repo), "fetch", "-q", "bottle/sort-it")   # and still reads the lane
        listed = self.ok("list")
        self.assertIn("sort-it", listed)
        self.assertNotIn("fix-it ", listed)

    def test_the_resume_makes_a_bottle_by_the_new_name(self) -> None:
        self.ok("pause", "fix-it")
        self.ok("rename", "fix-it", "sort-it")
        self.ok("shell", "sort-it")
        bottle = self.fake()["bottles"][f"{self.key}-sort-it"]
        self.assertIn("# Thread `sort-it`", bottle["context"])
        self.assertIn("agent:fetch=ns/sort-it-out,push=ns/sort-it-out", bottle["mounts"][0]["remotes"])
        self.assertEqual("sort-it", bottle["spec"]["thread"])

    def test_a_name_thats_taken_or_invalid_is_refused(self) -> None:
        self.ok("new", "other")
        self.ok("pause", "fix-it")
        for new, why in (("other", "already exists"), ("Sort It", "invalid thread name")):
            with self.subTest(new):
                result = self.folio("rename", "fix-it", new)
                self.assertNotEqual(0, result.returncode)
                self.assertIn(why, result.stderr)
        self.assertEqual(["refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1"], self.refs("refs/namespaces/fix-it-out/"))
