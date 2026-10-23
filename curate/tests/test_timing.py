"""The log: every command leaves a line saying when it ran and how long it took.

The point is the command that felt slow and is not slow the second time, so
nothing here is opt-in: the entry has to be there already when you go looking.
"""

import os
import re
import subprocess

from .support import CURATE, CurateTestCase

ENTRY = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}\S* pid \d+ "
                   r"\d+\.\d{3}s \(subprocesses \d+\.\d{3}s in \d+\) "
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
                       env={**os.environ, "CURATE_SLOW": slow})

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
        """`git add` in review mode is the user's, and runs post-index-change.
        The ones git fires under curate's own git return at once."""
        self.start(self.base)
        self.curate("review")
        self.repo.git("add", "f.txt")
        self.assertEqual([cmd for _, cmd in self.entries()],
                         [f"start --from {self.base}", "review",
                          "hook post-index-change 0 0"])

    def test_nothing_is_written_where_there_is_no_review(self) -> None:
        """Whether .git/curate exists is how curate knows there is anything
        here, so the log must never be what creates it."""
        self.curate("status")
        self.assertFalse(self.log.parent.exists())
