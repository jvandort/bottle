import subprocess

from folio import lanes
from tests.support import FolioTestCase, git


class LaneTest(FolioTestCase):
    """Real git: the lane's namespaces in the repo, and the remote that reads and writes them."""

    def setUp(self) -> None:
        super().setUp()
        lanes.register(self.repo, "t")
        self.head = git("-C", str(self.repo), "rev-parse", "HEAD")

    def bottle_pushes(self, branch: str, commit: str) -> None:
        """What the agent's `git push agent` lands as: a ref in the out lane."""
        git("-C", str(self.repo), "update-ref", f"refs/namespaces/t-out/refs/heads/{branch}", commit)

    def new_commit(self, message: str) -> str:
        """A commit in the repo's object store that no branch reaches, as a bottle's push leaves."""
        tree = git("-C", str(self.repo), "rev-parse", "HEAD^{tree}")
        return git("-C", str(self.repo), "commit-tree", tree, "-p", self.head, "-m", message)

    def test_the_bottle_is_served_each_lane_as_a_remote_of_its_own(self) -> None:
        self.assertEqual(lanes.bottle_remotes("t"), [
            "origin:fetch=upstream", "host:fetch=heads", "agent:fetch=ns/t-out,push=ns/t-out", "user:fetch=ns/t-in",
        ])

    def test_fetching_the_remote_reads_the_out_lane(self) -> None:
        commit = self.new_commit("agent work")
        self.bottle_pushes("genie/t-1", commit)
        git("-C", str(self.repo), "fetch", "-q", "bottle/t")
        self.assertEqual(git("-C", str(self.repo), "rev-parse", "bottle/t/genie/t-1"), commit)
        git("-C", str(self.repo), "switch", "-q", "genie/t-1")  # tracks it, with no more setup
        self.assertEqual(git("-C", str(self.repo), "rev-parse", "--abbrev-ref", "@{u}"), "bottle/t/genie/t-1")

    def test_pushing_to_the_remote_writes_the_in_lane(self) -> None:
        git("-C", str(self.repo), "push", "-q", "bottle/t", "main:refs/heads/handed")
        self.assertEqual(lanes.branches(self.repo, "t", lanes.IN), {"handed": self.head})
        self.assertEqual(lanes.branches(self.repo, "t"), {})

    def test_registering_twice_leaves_one_of_each(self) -> None:
        lanes.register(self.repo, "t")
        for key in ("url", "fetch", "uploadpack", "receivepack"):
            self.assertEqual(len(git("-C", str(self.repo), "config", "--get-all", f"remote.bottle/t.{key}").splitlines()), 1)

    def test_unintegrated_is_by_reachability(self) -> None:
        self.bottle_pushes("genie/t-1", self.new_commit("agent work"))
        self.bottle_pushes("merged", self.head)
        self.assertEqual(lanes.unintegrated(self.repo, "t"), ["genie/t-1"])
        git("-C", str(self.repo), "fetch", "-q", "bottle/t")
        self.assertEqual(lanes.unintegrated(self.repo, "t"), ["genie/t-1"])  # the remote's copy goes with the lane
        git("-C", str(self.repo), "branch", "kept", "bottle/t/genie/t-1")
        self.assertEqual(lanes.unintegrated(self.repo, "t"), [])

    def test_unsaved_is_whats_neither_in_the_lane_nor_the_repo(self) -> None:
        pushed = self.new_commit("pushed")
        self.bottle_pushes("genie/t-1", pushed)
        tips = {"genie/t-1": pushed, "main": self.head, "local": "1" * 40}
        self.assertEqual(lanes.unsaved(self.repo, "t", tips), ["local"])

    def test_forget_removes_both_lanes_and_the_remote(self) -> None:
        self.bottle_pushes("genie/t-1", self.new_commit("agent work"))
        git("-C", str(self.repo), "push", "-q", "bottle/t", "main:refs/heads/handed")
        git("-C", str(self.repo), "fetch", "-q", "bottle/t")
        lanes.forget(self.repo, "t")
        self.assertNotIn("namespaces", git("-C", str(self.repo), "for-each-ref", "--format=%(refname)"))
        self.assertNotIn("bottle/t", git("-C", str(self.repo), "for-each-ref", "--format=%(refname)"))
        self.assertEqual(git("-C", str(self.repo), "remote"), "")
        lanes.forget(self.repo, "t")  # and again, with nothing left: fine

    def test_another_threads_lane_is_left_alone(self) -> None:
        lanes.register(self.repo, "t-2")
        git("-C", str(self.repo), "update-ref", "refs/namespaces/t-2-out/refs/heads/x", self.head)
        lanes.forget(self.repo, "t")
        self.assertEqual(lanes.branches(self.repo, "t-2"), {"x": self.head})
        self.assertEqual(git("-C", str(self.repo), "remote"), "bottle/t-2")
