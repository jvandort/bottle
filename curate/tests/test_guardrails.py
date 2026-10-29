"""What happens when HEAD moves out from under a review."""

import os
import subprocess

from .support import CURATE, CurateTestCase, TIMEOUT


class Guardrails(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.git("switch", "-q", "-c", "elsewhere", self.base)
        self.repo.write("f.txt", "ELSEWHERE\ntwo\nthree\n")   # or nothing is at risk
        self.repo.commit_all("elsewhere's own work")
        self.repo.git("switch", "-q", "wip")
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("read")

    def approve_everything(self) -> None:
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "Everything")

    def test_an_unfinished_review_blocks_a_checkout_by_itself(self) -> None:
        """Git's own protection, which exists only while something is unreviewed."""
        result = subprocess.run(["git", "-C", str(self.repo.path), "switch", "elsewhere"],
                                capture_output=True, text=True, timeout=TIMEOUT)
        self.assertNotEqual(result.returncode, 0)

    def test_a_finished_review_does_not_and_that_is_the_hazard(self) -> None:
        self.approve_everything()
        result = subprocess.run(["git", "-C", str(self.repo.path), "switch", "elsewhere"],
                                capture_output=True, text=True, timeout=TIMEOUT)
        self.assertEqual(result.returncode, 0, "git no longer protects us here")

    def test_being_moved_out_of_review_mode_says_nothing(self) -> None:
        """Switching away is what you meant, so there is nothing to tell you."""
        self.approve_everything()
        result = subprocess.run(["git", "-C", str(self.repo.path), "switch", "elsewhere"],
                                capture_output=True, text=True, timeout=TIMEOUT)
        self.assertNotIn("curate", result.stderr)

    def test_and_stands_down_like_leaving_in_write_mode(self) -> None:
        """The reported annoyance: forgetting `curate write` cost a round trip."""
        self.approve_everything()
        self.repo.git("switch", "-q", "elsewhere")
        fields = self.repo.status_fields()
        self.assertEqual(fields["consistent"], "yes")
        self.assertNotIn("clean", fields, "still claiming the review it left")
        result = self.repo.curate("read")
        self.assertNotIn("moved it out from under", result.stderr)
        self.assertIn("no review here", result.stderr)

    def test_switching_from_one_review_to_another_says_nothing(self) -> None:
        """git switch between two reviews' working lines is how you move
        between them; no command was skipped, so there is nothing to report."""
        self.repo.curate("write")
        self.repo.git("switch", "-q", "elsewhere")
        self.start(self.base)
        self.repo.git("switch", "-q", "wip")
        result = self.repo.curate("read")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")

    def test_back_onto_the_clean_line_by_git_an_add_is_not_an_approval(self) -> None:
        """post-checkout stood the review down when git took HEAD away. Without
        that, the next `git add` on the clean line -- with no curate command in
        between to notice -- would be recorded over the approvals."""
        self.approve_everything()
        record = "refs/curate/curate%2Fwip"
        approved = self.repo.git("rev-parse", f"{record}^{{tree}}")
        self.repo.git("switch", "-q", "elsewhere")
        self.repo.git("switch", "-q", "curate/wip")
        self.repo.write("g.txt", "not approved\n")
        self.repo.git("add", "g.txt")
        self.assertEqual(self.repo.git("rev-parse", f"{record}^{{tree}}"), approved)

    def test_no_index_is_clobbered_after_being_moved(self) -> None:
        """The whole bug: the checkout is survivable, a blind mode switch is not."""
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.git("commit", "-q", "-m", "Everything")
        self.repo.git("switch", "-q", "elsewhere")
        self.repo.curate("write")
        self.repo.curate("read")
        self.repo.git("switch", "-q", "wip")
        result = self.repo.curate("read")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.index_tree(), approved, "approvals were destroyed")

    def test_approvals_are_recorded_as_they_are_made(self) -> None:
        """post-index-change, so nothing is lost between invocations."""
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        clean = self.repo.status_fields()["clean"][len("refs/heads/"):]
        slug = clean.replace("%", "%25").replace("/", "%2F")
        recorded = self.repo.git("rev-parse", f"refs/curate/{slug}^{{tree}}", check=False)
        self.assertEqual(recorded, approved)

    def test_recording_what_is_already_recorded_moves_nothing(self) -> None:
        """Every command records the approved set; only a change may move the
        ref, or its reflog says something happened when nothing did.

        Later, by the clock: a commit made again in the same second is the
        same commit, and would hide a ref being rewritten."""
        self.repo.git("add", "-A")
        ref = self.repo.git("for-each-ref", "--format=%(refname)", "refs/curate")
        before = self.repo.git("rev-parse", ref)
        later = subprocess.run([CURATE, "read"], cwd=self.repo.path,
                               capture_output=True, text=True,
                               env={**os.environ, "GIT_COMMITTER_DATE": "@4000000000 +0000"}, timeout=TIMEOUT)
        self.assertEqual(later.returncode, 0, later.stderr)
        self.assertEqual(self.repo.git("rev-parse", ref), before)

    def test_recovery_restores_the_last_recorded_approvals(self) -> None:
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.approve_everything()
        self.repo.git("switch", "-q", "elsewhere")
        self.repo.git("switch", "-q", "wip")
        result = self.repo.curate("read")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(self.repo.index_tree(), (approved, self.repo.tree_of("HEAD")))

    def test_status_is_safe_to_run_in_a_broken_state(self) -> None:
        self.approve_everything()
        self.repo.git("switch", "-q", "elsewhere")
        self.assertEqual(self.repo.curate("status", "--porcelain").returncode, 0)


class TornWrite(CurateTestCase):
    """`set_state` writes three files, and a crash can land between them.

    Nothing makes that atomic, and nothing needs to: the recorded mode and
    HEAD are compared before anything is touched, so a half-written state
    fails the comparison and is re-anchored on HEAD rather than believed.
    """

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("work on wip")
        self.state = self.repo.path / ".git" / "curate"

    def test_an_active_pointer_left_behind_is_re_anchored(self) -> None:
        """The torn write that start could leave: mode written, active not."""
        self.start(self.base)
        stale = (self.state / "active").read_text()

        self.repo.curate("write")
        self.repo.git("switch", "-q", "-c", "second", self.base)
        self.repo.write("g.txt", "other\n")
        self.repo.commit_all("work on second")
        self.assertEqual(self.repo.curate("start", "--from", self.base).returncode, 0)

        # As if the process died after writing `mode` and before `active`.
        (self.state / "active").write_text(stale)

        result = self.repo.curate("read")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"),
                         "refs/heads/curate/second")
        self.assertEqual(self.repo.status_fields()["consistent"], "yes")

    def test_a_mode_left_behind_is_re_anchored_too(self) -> None:
        """And the other half: active written, mode not."""
        self.start(self.base)
        self.repo.curate("read")
        (self.state / "mode").write_text("write\n")   # HEAD still says review

        result = self.repo.curate("write")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")
        self.assertEqual(self.repo.status_fields()["consistent"], "yes")
