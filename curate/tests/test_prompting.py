"""Asking, when there is a terminal to ask at."""

import os
import pty
import select
import signal
import time

from .support import CurateTestCase, CURATE


class Prompting(CurateTestCase):
    """On a terminal the tool asks for the base; off one, nothing changes."""

    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")

    def on_a_terminal(self, args: list[str], keys: list[str],
                      timeout: float = 20) -> str:
        """Run the tool on a real pty, type `keys`, return everything it printed.

        A pipe is not enough: the whole behaviour under test is gated on
        isatty, so the test has to provide an actual terminal.
        """
        pid, fd = pty.fork()
        if pid == 0:
            try:
                os.chdir(self.repo.path)
                os.execv(CURATE, [CURATE, *args])
            except BaseException:
                pass
            os._exit(127)
        output, deadline = b"", time.time() + timeout
        try:
            for key in keys:
                time.sleep(0.3)
                os.write(fd, key.encode())
            while time.time() < deadline:
                ready, _, _ = select.select([fd], [], [], 0.3)
                if not ready:
                    continue
                try:
                    chunk = os.read(fd, 4096)
                except OSError:      # EIO: the child closed the terminal
                    break
                if not chunk:
                    break
                output += chunk
        finally:
            os.close(fd)
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass
        return output.decode(errors="replace")

    def test_switching_to_review_mode_with_no_review_asks_and_starts_one(self) -> None:
        out = self.on_a_terminal(["review"], [f"{self.base}\n"])
        self.assertIn("Clean line starts at", out)
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "review")
        self.assertEqual(self.repo.git("rev-parse", fields["clean"]), self.base)

    def test_a_suggestion_names_the_branch_not_the_commit_message(self) -> None:
        """What you want to recognise is "main", not what was last done to it."""
        self.repo.git("branch", "main", self.base)     # something to fork from
        out = self.on_a_terminal(["start"], ["\n"])
        self.assertIn("Suggested: 'main'", out)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_a_branch_that_has_moved_on_is_not_called_by_its_name(self) -> None:
        """The merge base is no longer main's tip, so saying "main" would lie."""
        self.repo.git("branch", "main", self.base)
        moved = self.repo.git("commit-tree", self.repo.tree_of(self.base),
                              "-p", self.base, "-m", "someone else")
        self.repo.git("update-ref", "refs/heads/main", moved)
        out = self.on_a_terminal(["start"], ["\n"])
        self.assertIn("where 'wip' left 'main'", out)
        self.assertIn("1 commit since", out)
        self.assertEqual(self.repo.git("rev-parse", "curate/wip"), self.base)

    def test_an_answer_git_cannot_resolve_asks_again(self) -> None:
        out = self.on_a_terminal(["start"], ["nonsense\n", f"{self.base}\n"])
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
