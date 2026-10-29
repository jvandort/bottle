"""Switching between the two lines, and what each shows."""

from .support import CurateTestCase


class ModeSwitch(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.write("junk.txt", "debug\n")
        self.tip = self.repo.commit_all("their work")
        self.start(self.base)

    def test_review_mode_points_head_at_the_clean_line(self) -> None:
        self.repo.curate("read")
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "read")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), fields["clean"])

    def test_read_mode_is_stored_as_every_curate_knows_it(self) -> None:
        """`curate read` renamed the mode, not the state: a curate from before reads it as its own."""
        self.repo.curate("read")
        state = self.repo.path / ".git" / "curate"
        self.assertEqual((state / "mode").read_text().strip(), "review")
        self.assertEqual((state / "version").read_text().strip(), "1")
        self.assertEqual(self.repo.status_fields()["mode"], "read")

    def test_switching_does_not_touch_a_single_file(self) -> None:
        """The property the whole design rests on: no files move, so no reindex."""
        before = self.repo.mtimes()
        self.repo.curate("read")
        self.assertEqual(self.repo.mtimes(), before)
        self.repo.curate("write")
        self.assertEqual(self.repo.mtimes(), before)

    def test_unstaged_in_review_mode_is_the_review_to_do_list(self) -> None:
        self.repo.curate("read")
        self.assertEqual(sorted(self.repo.short_status()), [" M f.txt", "?? junk.txt"])

    def test_write_mode_hides_the_review_and_shows_only_your_own_edits(self) -> None:
        self.repo.curate("write")
        self.assertEqual(self.repo.short_status(), [])
        self.repo.write("f.txt", "ONE\ntwo\nMY EDIT\n")
        self.assertEqual(self.repo.short_status(), [" M f.txt"])

    def test_the_reflog_says_which_switch_was_which(self) -> None:
        """Which matters when a line has gone missing and you are reading it."""
        self.repo.curate("read")
        self.repo.curate("write")
        reflog = self.repo.git("reflog", "--format=%gs")
        self.assertIn("curate: read mode", reflog)
        self.assertIn("curate: write mode", reflog)

    def test_each_mode_keeps_its_own_staged_set(self) -> None:
        self.repo.curate("write")
        self.repo.write("mine.txt", "mine\n")
        self.repo.git("add", "mine.txt")
        self.repo.curate("read")
        self.repo.git("add", "f.txt")
        self.repo.curate("write")
        self.assertIn("A  mine.txt", self.repo.short_status(), "write staging was clobbered")


class Switching(CurateTestCase):
    """`curate switch` toggles, so there is one key to bind."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)

    def test_switch_toggles_both_ways(self) -> None:
        self.repo.curate("switch")
        self.assertEqual(self.repo.status_fields()["mode"], "read")
        self.repo.curate("switch")
        self.assertEqual(self.repo.status_fields()["mode"], "write")

    def test_there_is_no_end_verb(self) -> None:
        """It was a second name for `write`, and one name is enough."""
        self.assertEqual(self.repo.curate("end").returncode, 1)


class SwitchFailure(CurateTestCase):
    """A mode switch that cannot finish must leave nothing half-done.

    Every failure but one leaves HEAD and the recorded mode disagreeing, which
    the invariant catches loudly. A failure to move HEAD would not: both would
    still say the old mode while the live index had already become the new
    one's -- consistent, and wrong.
    """

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        # g.txt is never approved, so the two index files differ. Without it
        # the working line's tree equals the approved one and this test cannot
        # tell whether anything moved.
        self.repo.write("g.txt", "new\n")
        self.repo.commit_all("their work")
        self.start(self.base)

    def test_a_held_index_lock_is_waited_out_rather_than_raced(self) -> None:
        """An editor's git holding the index is the one thing a switch must
        not move under."""
        lock = self.repo.path / ".git" / "index.lock"
        lock.write_text("")
        result = self.repo.curate("read")
        lock.unlink()
        self.assertEqual(result.returncode, 1)
        self.assertIn("holds .git/index.lock", result.stderr)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")
        self.assertEqual(self.repo.curate("read").returncode, 0)

    def test_a_line_deleted_by_hand_is_not_switched_to(self) -> None:
        self.repo.git("branch", "-D", "curate/wip")
        result = self.repo.curate("read")
        self.assertEqual(result.returncode, 1)
        self.assertIn("'curate/wip' does not exist", result.stderr)
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")

    def test_a_failed_switch_leaves_the_indexes_where_they_were(self) -> None:
        self.repo.curate("read")
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.curate("write")
        live = self.repo.index_tree()

        # Make the ref move fail, and only that: holding HEAD.lock stops
        # symbolic-ref without stopping anything before it.
        lock = self.repo.path / ".git" / "HEAD.lock"
        lock.write_text("")
        result = self.repo.curate("read")
        lock.unlink()

        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.repo.index_tree(), live, "the live index moved")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), "refs/heads/wip")
        self.assertEqual(self.repo.status_fields()["mode"], "write")

        self.repo.curate("read")
        self.assertEqual(self.repo.index_tree(), approved, "approvals were lost")
