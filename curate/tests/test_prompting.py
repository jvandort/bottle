"""Asking, when there is a terminal to ask at."""

import sys
import unittest
from pathlib import Path

from .support import CurateTestCase

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from curate.pick import Picker, Row


class Prompting(CurateTestCase):
    """On a terminal the tool asks for the base; anywhere else it takes a flag."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")

    def test_switching_to_review_mode_with_no_review_asks_and_starts_one(self) -> None:
        out = self.on_a_terminal(["review"], ["/", self.base, "\r"])
        self.assertIn("Clean line for 'wip' starts at", out)
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "review")
        self.assertEqual(self.repo.git("rev-parse", fields["clean"]), self.base)

    def test_a_suggestion_names_the_branch_not_the_commit_message(self) -> None:
        """What you want to recognise is "main", not what was last done to it."""
        self.repo.git("branch", "main", self.base)     # something to fork from
        out = self.on_a_terminal(["start"], ["\r"])
        self.assertIn("Suggested: 'main'", out)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_a_branch_that_has_moved_on_is_not_called_by_its_name(self) -> None:
        """The merge base is no longer main's tip, so saying "main" would lie."""
        self.repo.git("branch", "main", self.base)
        moved = self.repo.git("commit-tree", self.repo.tree_of(self.base),
                              "-p", self.base, "-m", "someone else")
        self.repo.git("update-ref", "refs/heads/main", moved)
        out = self.on_a_terminal(["start"], ["\r"])
        self.assertIn("where 'wip' left 'main'", out)
        self.assertIn("1 commit since", out)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_an_answer_git_cannot_resolve_says_so_and_keeps_asking(self) -> None:
        out = self.on_a_terminal(["start"], ["/", "nonsense", "\r",
                                             "/", self.base, "\r"])
        self.assertIn("cannot resolve nonsense", out)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_eof_cancels_and_creates_nothing(self) -> None:
        out = self.on_a_terminal(["review"], ["\x04"])
        self.assertIn("cancelled", out)
        self.assertEqual(self.repo.git("branch", "--list", "curate/wip"), "")
        self.assertFalse((self.repo.path / ".git" / "curate" / "sessions").exists())

    def test_off_a_terminal_it_refuses_rather_than_blocking(self) -> None:
        """A pipe has nobody to answer, so the old refusal has to survive."""
        result = self.repo.curate("review")
        self.assertEqual(result.returncode, 1)
        self.assertIn("curate start", result.stderr)
        self.assertEqual(self.repo.git("branch", "--list", "curate/wip"), "")

    def test_a_terminal_curses_cannot_drive_is_no_terminal_at_all(self) -> None:
        """One way of asking, not a second one kept alive for dumb terminals."""
        out = self.on_a_terminal(["review"], [], term="dumb")
        self.assertIn("curate start --from", out)
        self.assertEqual(self.repo.git("branch", "--list", "curate/wip"), "")


class Picking(CurateTestCase):
    """The base as a list to move through, on a terminal curses can drive."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.git("branch", "main", self.base)
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.first = self.repo.commit_all("their work")
        self.repo.write("f.txt", "ONE\nTWO\nthree\n")
        self.repo.commit_all("more of it")

    def test_enter_takes_the_best_guess(self) -> None:
        out = self.on_a_terminal(["start"], ["\r"])
        self.assertIn("best guess", out)
        self.assertIn(f"Clean line starts at {self.base[:8]} base", out)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_the_list_names_the_branches_on_each_commit(self) -> None:
        out = self.on_a_terminal(["start"], ["q"])
        self.assertIn("main", out)
        self.assertNotIn("curate/", out)

    def test_k_moves_up_the_line(self) -> None:
        self.on_a_terminal(["start"], ["k", "\r"])
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.first)

    def test_arrow_keys_move_too(self) -> None:
        self.on_a_terminal(["start"], ["\x1bOA", "\r"])     # up, as xterm sends it in keypad mode
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.first)

    def test_slash_takes_any_revision(self) -> None:
        self.on_a_terminal(["start"], ["/", "wip~1", "\r"])
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.first)

    def test_q_cancels_and_creates_nothing(self) -> None:
        out = self.on_a_terminal(["review"], ["q"])
        self.assertIn("cancelled", out)
        self.assertEqual(self.repo.git("branch", "--list", "curate/wip"), "")
        self.assertFalse((self.repo.path / ".git" / "curate" / "sessions").exists())


class PickerKeys(unittest.TestCase):
    """What a key does to the list, without a terminal in the way."""

    def rows(self, n: int) -> list[Row]:
        return [Row([str(i)], value=str(i)) for i in range(n)]

    def test_moving_stops_at_the_ends(self) -> None:
        p = Picker(self.rows(3), cursor=0)
        p.key("k")
        self.assertEqual(p.cursor, 0)
        for _ in range(5):
            p.key("j")
        self.assertEqual(p.cursor, 2)

    def test_the_list_scrolls_to_keep_the_cursor_on_screen(self) -> None:
        p = Picker(self.rows(20), cursor=15, height=5)
        self.assertEqual(p.top, 11)
        p.key("g")
        self.assertEqual((p.cursor, p.top), (0, 0))

    def test_enter_on_an_expanding_row_puts_its_rows_in_its_place(self) -> None:
        more = Row(["<show local>"], expand=lambda: self.rows(2))
        p = Picker([Row(["a"], value="a"), more], cursor=1)
        p.key("\n")
        self.assertIsNone(p.chosen)
        self.assertEqual([r.value for r in p.rows], ["a", "0", "1"])
        p.key("\n")
        self.assertEqual(p.chosen, "0")

    def test_typing_something_unknown_says_why_and_keeps_the_list(self) -> None:
        p = Picker(self.rows(2), typed=lambda t: (None, f"cannot resolve {t}"))
        for k in "/xy\x7f":
            p.key(k)
        self.assertEqual(p.footer(), "/x")
        p.key("\n")
        self.assertFalse(p.done)
        self.assertEqual(p.footer(), "cannot resolve x")

    def test_a_list_with_nothing_to_type_ignores_slash(self) -> None:
        p = Picker(self.rows(2))
        p.key("/")
        self.assertIsNone(p.typing)

    def test_a_row_with_a_prompt_asks_for_text_when_chosen(self) -> None:
        new = Row(["<new branch>"], prompt="new branch: ")
        p = Picker([new, *self.rows(2)], typed=lambda t: (f"new:{t}", ""))
        p.key("\n")
        self.assertEqual(p.footer(), "new branch: ")
        for k in "x\n":
            p.key(k)
        self.assertEqual(p.chosen, "new:x")


class Checkboxes(unittest.TestCase):
    """Every row has to be passed before the list can be confirmed."""

    def picker(self) -> Picker:
        return Picker([Row([a], value=a) for a in "abc"], checkboxes=True)

    def test_space_toggles_the_row_under_the_cursor(self) -> None:
        p = self.picker()
        for k in " j  j":                # a on; b on, then off again
            p.key(k)
        self.assertEqual(p.checked, ["a"])

    def test_enter_before_the_last_row_says_why_and_does_nothing(self) -> None:
        p = self.picker()
        p.key("j")
        p.key("\n")
        self.assertFalse(p.done)
        self.assertIn("every row", p.footer())

    def test_enter_on_the_last_row_confirms_what_is_checked(self) -> None:
        p = self.picker()
        for k in "j jj \n":
            p.key(k)
        self.assertTrue(p.done)
        self.assertEqual(p.checked, ["b", "c"])

    def test_nothing_checked_is_an_answer_too(self) -> None:
        p = self.picker()
        for k in "jj\n":
            p.key(k)
        self.assertTrue(p.done)
        self.assertEqual(p.checked, [])
