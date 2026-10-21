import curses
import io
from contextlib import redirect_stdout
from unittest import mock

from folio import cli, picker, repos
from folio.picker import NEW, Choice, Picker
from tests.support import FOLIO, FolioTestCase

J, K, ENTER, Q, ESC = ord("j"), ord("k"), 10, ord("q"), 27


class Terminal(io.StringIO):
    """Captured output that passes for a terminal, as the picker needs."""

    def isatty(self) -> bool:
        return True


def picker_of(n: int, new: bool = False) -> Picker:
    return Picker("title", ([NEW] if new else []) + [Choice(f"t{i}", f"t{i}") for i in range(n)])


class PickerTest(FolioTestCase):
    def test_moves_and_stops_at_the_ends(self) -> None:
        p = picker_of(3)
        for key, index in ((K, 0), (J, 1), (curses.KEY_DOWN, 2), (J, 2), (curses.KEY_UP, 1), (ord("G"), 2), (ord("g"), 0)):
            self.assertEqual(p.press(key), (False, None))
            self.assertEqual(p.index, index, chr(key) if key < 128 else key)

    def test_enter_chooses_and_q_or_esc_leave(self) -> None:
        p = picker_of(3)
        p.press(J)
        self.assertEqual(p.press(ENTER), (True, Choice("t1", "t1")))
        for key in (Q, ESC, 3):
            self.assertEqual(picker_of(3).press(key), (True, None))

    def test_enter_on_nothing_does_nothing(self) -> None:
        self.assertEqual(picker_of(0).press(ENTER), (False, None))

    def test_the_new_entry_comes_first(self) -> None:
        self.assertEqual(picker_of(2, new=True).press(ENTER), (True, NEW))

    def test_draws_the_title_and_marks_the_current_choice(self) -> None:
        p = picker_of(2)
        p.press(J)
        self.assertEqual(p.lines(10), [("title", False), ("", False), ("  t0", False), ("> t1", True)])

    def test_scrolls_to_keep_the_current_choice_in_view(self) -> None:
        p = picker_of(10)
        for _ in range(6):
            p.press(J)
        shown = [text for text, _ in p.lines(5)[2:]]  # three rows of choices
        self.assertEqual(shown, ["  t4", "  t5", "> t6"])
        for _ in range(5):
            p.press(K)
        self.assertEqual([text for text, _ in p.lines(5)[2:]], ["> t1", "  t2", "  t3"])


class ChoosingTest(FolioTestCase):
    """A command given no thread asks the picker, on a terminal; and says why not, without one."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "older")
        self.ok("new", "newer")
        self.repo_ = repos.current(self.repo)

    def run_cli(self, *argv: str, chosen=None, typed: str = "") -> tuple[int, list[Choice], mock.Mock]:
        shown = []

        def pick(title, choices):
            shown.extend(choices)
            return chosen(choices) if callable(chosen) else chosen

        with mock.patch("folio.repos.current", return_value=self.repo_), \
                mock.patch("sys.stdin.isatty", return_value=True), redirect_stdout(Terminal()), \
                mock.patch.object(picker, "pick", side_effect=pick), mock.patch("builtins.input", return_value=typed), \
                mock.patch("folio.engine.attach") as attach:
            code = cli.main(list(argv))
        return code, shown, attach

    def test_without_a_terminal_it_says_to_name_one(self) -> None:
        for command in ("claude", "shell", "pause", "delete"):
            with self.subTest(command):
                result = self.folio(command)
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"give the thread: `folio {command} THREAD`", result.stderr)

    def test_lists_the_threads_most_recent_first(self) -> None:
        _, shown, _ = self.run_cli("pause")
        self.assertEqual([c.thread for c in shown], ["newer", "older"])
        self.assertRegex(shown[0].label, r"^newer  running   just now$")

    def test_attaches_to_the_one_chosen(self) -> None:
        code, shown, attach = self.run_cli("claude", chosen=lambda choices: choices[2])
        self.assertEqual(shown[0], picker.NEW)
        self.assertEqual(attach.call_args.args[0], f"{self.repo_.key}-older")

    def test_a_new_thread_from_the_picker_is_made_and_attached_to(self) -> None:
        code, _, attach = self.run_cli("shell", chosen=picker.NEW, typed="fresh")
        self.assertEqual(attach.call_args.args, (f"{self.repo_.key}-fresh", ["tmux", "attach", "-d", "-t", "=shell"]))

    def test_only_claude_and_shell_offer_a_new_thread(self) -> None:
        for command in ("pause", "delete"):
            _, shown, _ = self.run_cli(command)
            self.assertNotIn(picker.NEW, shown)

    def test_leaving_without_choosing_does_nothing(self) -> None:
        for command in ("claude", "pause", "delete"):
            with self.subTest(command):
                code, _, attach = self.run_cli(command, chosen=None)
                self.assertEqual(code, 0)
                attach.assert_not_called()
        self.assertEqual(len(self.fake()["bottles"]), 2)
        code, _, attach = self.run_cli("claude", chosen=picker.NEW, typed="")  # a new thread, but no name
        attach.assert_not_called()

    def test_with_no_threads_only_claude_and_shell_have_anything_to_offer(self) -> None:
        self.ok("delete", "older", "--yes")
        self.ok("delete", "newer", "--yes")
        out = io.StringIO()
        with mock.patch("folio.repos.current", return_value=self.repo_), mock.patch("sys.stdin.isatty", return_value=True), \
                redirect_stdout(Terminal()), mock.patch("sys.stderr", out):
            self.assertEqual(cli.main(["pause"]), 1)
        self.assertIn("has no threads; start one with `folio new THREAD`", out.getvalue())
        _, shown, _ = self.run_cli("claude")
        self.assertEqual(shown, [picker.NEW])


class TerminalTest(FolioTestCase):
    """The real curses loop, on a pseudo-terminal: keys in, a choice out."""

    def drive(self, argv: list[str], keys: bytes) -> tuple[int, bytes]:
        import os
        import pty
        import select
        import sys
        import time

        pid, fd = pty.fork()
        if pid == 0:  # the child: folio, on the terminal
            os.chdir(self.repo)
            os.execve(sys.executable, [sys.executable, str(FOLIO), *argv],
                      {**self.env, "TERM": "xterm-256color", "LINES": "24", "COLUMNS": "80"})
        out, deadline, sent = b"", time.monotonic() + 20, False
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([fd], [], [], 0.2)
                if ready:
                    try:
                        chunk = os.read(fd, 4096)
                    except OSError:
                        break  # the child has gone
                    if not chunk:
                        break
                    out += chunk
                if not sent and b"choose a thread" in out:
                    for key in keys:
                        os.write(fd, bytes([key]))
                        time.sleep(0.05)
                    sent = True
            _, status = os.waitpid(pid, 0)
        finally:
            try:
                os.kill(pid, 9)
            except ProcessLookupError:
                pass
            os.close(fd)
        return os.waitstatus_to_exitcode(status), out

    def test_choosing_with_keys(self) -> None:
        self.ok("new", "older")
        self.ok("new", "newer")
        code, out = self.drive(["claude"], b"jj\r")  # past + new thread and newer, to older
        self.assertEqual(code, 0, out[-500:])
        self.assertIn(b"older", out)
        self.assertEqual(self.calls("exec")[-1][1:], [f"{repos.current(self.repo).key}-older", "--", "tmux", "attach",
                                                      "-d", "-t", "=claude"])

    def test_leaving_with_q(self) -> None:
        self.ok("new", "only")
        code, _ = self.drive(["pause"], b"q")
        self.assertEqual(code, 0)
        self.assertEqual(self.calls("delete"), [])
