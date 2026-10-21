"""The picker: choose a thread from a list, when a command is given none.

The repo's threads, most recently active first, with their state. Move with
j/k or the arrow keys (g and G for the ends), Enter to choose, q or Esc to
leave without choosing. A picker can have an entry at the top for a new
thread.

What it does with a key is Picker's, and testable without a terminal; what it
draws is `lines()`. curses only shows those and reads keys.
"""

import curses
import os
from dataclasses import dataclass

ENTER = (10, 13, curses.KEY_ENTER)
DOWN = (ord("j"), curses.KEY_DOWN, 14)  # 14: ^N
UP = (ord("k"), curses.KEY_UP, 16)  # 16: ^P
FIRST = (ord("g"), curses.KEY_HOME)
LAST = (ord("G"), curses.KEY_END)
LEAVE = (ord("q"), 27, 3)  # 27: Esc, 3: ^C


@dataclass(frozen=True)
class Choice:
    label: str
    thread: str | None  # None: a new thread


NEW = Choice("+ new thread", None)


class Picker:
    def __init__(self, title: str, choices: list[Choice]) -> None:
        self.title = title
        self.choices = choices
        self.index = 0
        self.top = 0  # the first choice shown, when they don't all fit

    def press(self, key: int) -> tuple[bool, Choice | None]:
        """Act on a key: (done, what was chosen, or None for nothing)."""
        last = len(self.choices) - 1
        if key in ENTER and self.choices:
            return True, self.choices[self.index]
        if key in LEAVE:
            return True, None
        if key in DOWN:
            self.index = min(self.index + 1, last)
        elif key in UP:
            self.index = max(self.index - 1, 0)
        elif key in FIRST:
            self.index = 0
        elif key in LAST:
            self.index = max(last, 0)
        return False, None

    def lines(self, height: int) -> list[tuple[str, bool]]:
        """What to draw in `height` rows: (text, whether it's the current choice)."""
        rows = max(height - 2, 1)  # the title, and a blank line under it
        if self.index < self.top:
            self.top = self.index
        elif self.index >= self.top + rows:
            self.top = self.index - rows + 1
        shown = self.choices[self.top:self.top + rows]
        return [(self.title, False), ("", False)] + [
            (f"{'>' if self.top + i == self.index else ' '} {choice.label}", self.top + i == self.index)
            for i, choice in enumerate(shown)
        ]


def pick(title: str, choices: list[Choice]) -> Choice | None:
    """Show the picker on this terminal; what was chosen, or None."""
    os.environ.setdefault("ESCDELAY", "25")  # Esc on its own, without curses waiting a second for more
    picker = Picker(title, choices)

    def loop(screen: "curses.window") -> Choice | None:
        curses.curs_set(0)
        while True:
            screen.erase()
            height, width = screen.getmaxyx()
            for row, (text, current) in enumerate(picker.lines(height)):
                screen.addnstr(row, 0, text, width - 1, curses.A_REVERSE if current else curses.A_NORMAL)
            screen.refresh()
            done, choice = picker.press(screen.getch())
            if done:
                return choice

    try:
        return curses.wrapper(loop)
    except KeyboardInterrupt:
        return None
