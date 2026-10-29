"""The log: every command leaves a line saying when it ran and how long it took.

The point is the command that felt slow and is not slow the second time, so
nothing here is opt-in: the entry has to be there already when you go looking.
"""

import os
import re
import subprocess

from .support import CURATE, CurateTestCase, TIMEOUT

ENTRY = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}\S* pid \d+ "
                   r"\d+\.\d{3}s \((?:waited for you \d+\.\d{3}s; )?subprocesses \d+\.\d{3}s in \d+\) "
                   r"exit (\S+): curate (.*)$")


class Timing(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.log = self.repo.path / ".git" / "curate" / "log"

    def entries(self) -> list[tuple[str, str]]:
        return [(m[1], m[2]) for line in self.log.read_text().splitlines()
                if (m := ENTRY.match(line))]

    def curate(self, *args: str, slow: str = "1") -> None:
        subprocess.run([CURATE, *args], cwd=self.repo.path, capture_output=True,
                       env={**os.environ, "CURATE_SLOW": slow}, timeout=TIMEOUT)

    def test_every_command_is_logged_with_when_and_how_long(self) -> None:
        self.start(self.base)
        self.curate("status")
        self.curate("no-such-command")
        self.assertEqual(self.entries(), [("0", f"start --from {self.base}"),
                                          ("0", "status"),
                                          ("1", "no-such-command")])

    def test_a_slow_one_lists_every_git_it_ran(self) -> None:
        self.start(self.base)
        self.curate("status", slow="0")
        detail = self.log.read_text().splitlines()[-1]
        self.assertRegex(detail, r"^    \+\d+\.\d{3}s \d+\.\d{3}s git ")

    def test_a_hook_you_fired_is_logged_and_curates_own_are_not(self) -> None:
        """`git add` in read mode is the user's, and runs post-index-change.
        The ones git fires under curate's own git return at once."""
        self.start(self.base)
        self.curate("read")
        self.repo.git("add", "f.txt")
        self.assertEqual([cmd for _, cmd in self.entries()],
                         [f"start --from {self.base}", "read",
                          "hook post-index-change 0 0"])

    def test_nothing_is_written_where_there_is_no_review(self) -> None:
        """Whether .git/curate exists is how curate knows there is anything
        here, so the log must never be what creates it."""
        self.curate("status")
        self.assertFalse(self.log.parent.exists())

    def test_waiting_for_an_answer_is_not_counted_as_slow(self) -> None:
        """The case that prompted it: 19s for a `curate read`, of which 14
        were the base list waiting for someone to choose."""
        self.on_a_terminal(["start"], ["\r", "\r"])
        line = self.log.read_text().splitlines()[-1]
        found = re.search(r"\(waited for you (\d+\.\d{3})s; subprocesses \S+ in (\d+)\)", line)
        assert found, line
        self.assertGreaterEqual(float(found[1]), 0.25, "the pause before the key")
        self.assertGreater(int(found[2]), 5, "the git it ran, not the two waits")
        self.assertEqual(len(self.log.read_text().splitlines()), 1,
                         "the wait alone does not make it slow")

    def test_a_full_log_is_kept_as_log_1_and_started_again(self) -> None:
        self.start(self.base)
        self.log.write_text("x" * (1 << 20) + "\n")
        self.curate("status")
        self.assertEqual((self.log.parent / "log.1").stat().st_size, (1 << 20) + 1)
        self.assertEqual([cmd for _, cmd in self.entries()], ["status"])

    def test_a_curate_slow_that_is_not_a_number_is_the_default(self) -> None:
        self.start(self.base)
        result = subprocess.run([CURATE, "status"], cwd=self.repo.path,
                                capture_output=True, text=True,
                                env={**os.environ, "CURATE_SLOW": "soon"}, timeout=TIMEOUT)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([cmd for _, cmd in self.entries()][-1], "status")
