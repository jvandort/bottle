from tests import state as kept
from tests.support import FolioTestCase


class ChoosingTest(FolioTestCase):
    """A command given no thread asks the picker, on a terminal; and says why not, without one."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "older")
        self.ok("new", "newer")
        self.key = kept.repo(self.repo).key

    def pick(self, command: str, keys: bytes, *more: tuple[bytes, bytes]) -> tuple[int, bytes]:
        """`folio COMMAND`, given no thread, on a terminal: `keys` typed in the picker, then `more`."""
        return self.on_terminal([command], [(b"choose a thread", keys), *more])

    def attached(self) -> list[str] | None:
        """The bottle attached to last, if any was."""
        execs = [c for c in self.calls("exec") if "attach" in c]
        return execs[-1][1] if execs else None

    def test_without_a_terminal_it_says_to_name_one(self) -> None:
        for command in ("claude", "shell", "pause", "delete"):
            with self.subTest(command):
                result = self.folio(command)
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"give the thread: `folio {command} THREAD`", result.stderr)

    def test_lists_the_threads_most_recent_first(self) -> None:
        code, out = self.pick("pause", b"q")
        self.assertEqual(code, 0, out)
        self.assertLess(out.index(b"newer  running   just now"), out.index(b"older  running   just now"))

    def test_attaches_to_the_one_chosen(self) -> None:
        code, out = self.pick("claude", b"jj\r")   # past + new thread and newer, to older
        self.assertEqual(code, 0, out)
        self.assertLess(out.index(kept.NEW.encode()), out.index(b"newer"))
        self.assertEqual(self.attached(), f"{self.key}-older")

    def test_a_new_thread_from_the_picker_is_made_and_attached_to(self) -> None:
        code, out = self.pick("shell", b"\r", (b"The new thread's name: ", b"fresh\r"))
        self.assertEqual(code, 0, out)
        self.assertEqual(self.calls("exec")[-1], ["exec", f"{self.key}-fresh", "--", "sh", "-c", kept.ATTACH, "attach",
                                                  "shell", kept.session_command("shell"), "/workspace"])

    def test_only_claude_and_shell_offer_a_new_thread(self) -> None:
        for command in ("pause", "delete"):
            with self.subTest(command):
                _, out = self.pick(command, b"q")
                self.assertNotIn(kept.NEW.encode(), out)

    def test_leaving_without_choosing_does_nothing(self) -> None:
        for command in ("claude", "pause", "delete"):
            with self.subTest(command):
                code, out = self.pick(command, b"q")
                self.assertEqual(code, 0, out)
                self.assertIsNone(self.attached())
        self.assertEqual(len(self.fake()["bottles"]), 2)
        self.pick("claude", b"\r", (b"The new thread's name: ", b"\r"))  # a new thread, but no name
        self.assertIsNone(self.attached())
        self.assertEqual(len(self.fake()["bottles"]), 2)

    def test_with_no_threads_only_claude_and_shell_have_anything_to_offer(self) -> None:
        self.ok("delete", "older", "--yes")
        self.ok("delete", "newer", "--yes")
        code, out = self.on_terminal(["pause"], [])
        self.assertEqual(code, 1)
        self.assertIn(b"has no threads; start one with `folio new THREAD`", out)
        _, out = self.pick("claude", b"q")
        self.assertIn(kept.NEW.encode(), out)


class TerminalTest(FolioTestCase):
    """The picker on a pseudo-terminal: keys in, a choice out."""

    def test_choosing_with_keys(self) -> None:
        self.ok("new", "older")
        self.ok("new", "newer")
        code, out = self.on_terminal(["claude"], [(b"choose a thread", b"jj\r")])  # past + new thread and newer
        self.assertEqual(code, 0, out[-500:])
        self.assertIn(b"older", out)
        self.assertEqual(self.calls("exec")[-1][1:], [f"{kept.repo(self.repo).key}-older", "--", "sh", "-c",
                                                      kept.ATTACH, "attach", "claude", kept.session_command("claude"),
                                                      "/workspace"])

    def test_leaving_with_q(self) -> None:
        self.ok("new", "only")
        code, _ = self.on_terminal(["pause"], [(b"choose a thread", b"q")])
        self.assertEqual(code, 0)
        self.assertEqual(self.calls("delete"), [])
