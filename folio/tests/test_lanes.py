import subprocess
from pathlib import Path

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


class DeleteTest(FolioTestCase):
    """A delete pushed to the lane remote: the agent's branch goes too, if it's still what was fetched."""

    bottle_pushes = LaneTest.bottle_pushes
    new_commit = LaneTest.new_commit

    def setUp(self) -> None:
        super().setUp()
        lanes.register(self.repo, "t")
        self.head = git("-C", str(self.repo), "rev-parse", "HEAD")
        self.work = self.new_commit("agent work")
        self.bottle_pushes("genie/t-1", self.work)
        git("-C", str(self.repo), "fetch", "-q", "bottle/t")

    def lane(self) -> dict[str, str]:
        return lanes.branches(self.repo, "t")

    def tracking(self) -> str:
        return git("-C", str(self.repo), "for-each-ref", "--format=%(refname)", "refs/remotes/bottle/t/")

    def delete(self, branch: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["git", "push", "bottle/t", f":refs/heads/{branch}"],
                              cwd=self.repo, capture_output=True, text=True)

    def test_the_agents_branch_goes_with_its_tracking_ref(self) -> None:
        result = self.delete("genie/t-1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("folio: deleted genie/t-1 from the agent's lane", result.stderr)
        self.assertEqual((self.lane(), self.tracking()), ({}, ""))

    def test_one_the_agent_pushed_to_since_is_kept_and_the_push_refused(self) -> None:
        later = self.new_commit("more")
        self.bottle_pushes("genie/t-1", later)
        result = self.delete("genie/t-1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("folio: kept genie/t-1: the agent has pushed to it since you fetched it", result.stderr)
        self.assertEqual(self.lane(), {"genie/t-1": later})

    def test_one_alongside_a_handover_goes_too(self) -> None:
        result = subprocess.run(["git", "push", "bottle/t", "main", ":refs/heads/genie/t-1"],
                                cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.lane(), {})
        self.assertIn("main", lanes.branches(self.repo, "t", lanes.IN))

    def test_curate_integrating_it_deletes_it(self) -> None:
        """The whole way: curate reviews the branch, integrates it, and deletes it from the lane."""
        curate = str(Path(lanes.__file__).resolve().parents[2] / "bin" / "curate")
        run = lambda *args: subprocess.run([curate, *args], cwd=self.repo, capture_output=True, text=True, env=self.env)
        git("-C", str(self.repo), "switch", "-q", "genie/t-1")
        for args in (["start", "--from", self.head], ["read"]):
            result = run(*args)
            self.assertEqual(result.returncode, 0, result.stderr)
        git("-C", str(self.repo), "add", "-A")
        result = run("integrate", "--into", "main")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Deleted genie/t-1 from bottle/t.", result.stdout)
        self.assertEqual((self.lane(), self.tracking()), ({}, ""))
