"""Choosing one thing from a list, on a terminal.

Two halves. `Picker` is what the list is and what a key does to it, and knows
nothing about terminals, so tests drive it directly. `choose` draws it with
curses and feeds it keys.
"""

import curses
import os
from dataclasses import dataclass
from typing import Callable

from .errors import Refused

# What `typed` answers: the value, or why not.
Typed = Callable[[str], tuple[str | None, str]]


@dataclass
class Row:
    cells: list[str]                 # columns, aligned across rows
    value: str | None = None         # what choosing it returns
    tag: str = ""                    # a label: "best guess"
    expand: Callable[[], list["Row"]] | None = None   # chosen, it becomes these


@dataclass
class Picker:
    rows: list[Row]
    cursor: int = 0
    top: int = 0                     # the first row on screen
    height: int = 10                 # rows on screen
    typing: str | None = None        # the text after `/`, while typing
    message: str = ""                # one line under the list
    chosen: str | None = None
    cancelled: bool = False
    typed: Typed | None = None

    def __post_init__(self) -> None:
        self.cursor = max(0, min(self.cursor, len(self.rows) - 1))
        self.scroll()

    @property
    def done(self) -> bool:
        return self.chosen is not None or self.cancelled

    def scroll(self) -> None:
        """Keep the cursor on screen, with the list starting where it can."""
        if self.cursor < self.top:
            self.top = self.cursor
        elif self.cursor >= self.top + self.height:
            self.top = self.cursor - self.height + 1

    def move(self, by: int) -> None:
        self.cursor = max(0, min(self.cursor + by, len(self.rows) - 1))
        self.message = ""
        self.scroll()

    def enter(self) -> None:
        row = self.rows[self.cursor]
        if row.expand is not None:
            self.rows[self.cursor:self.cursor + 1] = row.expand()
            self.cursor = min(self.cursor, len(self.rows) - 1)
            self.scroll()
        elif row.value is not None:
            self.chosen = row.value

    def key(self, k: str) -> None:
        """One key: a character, or a curses name such as KEY_UP."""
        if self.typing is not None:
            self.type_key(k)
        elif k in ("j", "KEY_DOWN"):
            self.move(1)
        elif k in ("k", "KEY_UP"):
            self.move(-1)
        elif k in ("KEY_NPAGE", " "):
            self.move(self.height)
        elif k == "KEY_PPAGE":
            self.move(-self.height)
        elif k in ("g", "KEY_HOME"):
            self.move(-len(self.rows))
        elif k in ("G", "KEY_END"):
            self.move(len(self.rows))
        elif k in ("\n", "\r", "KEY_ENTER"):
            self.enter()
        elif k in ("q", "\x1b", "\x03", "\x04"):
            self.cancelled = True
        elif k == "/" and self.typed is not None:
            self.typing, self.message = "", ""

    def type_key(self, k: str) -> None:
        assert self.typing is not None and self.typed is not None
        if k in ("\n", "\r", "KEY_ENTER"):
            if not self.typing:
                self.typing = None
                return
            value, why = self.typed(self.typing)
            if value is not None:
                self.chosen = value
            else:
                self.message, self.typing = why, None
        elif k in ("\x1b", "\x03"):
            self.typing = None
        elif k in ("KEY_BACKSPACE", "\x7f", "\x08"):
            self.typing = self.typing[:-1]
        elif len(k) == 1 and k.isprintable():
            self.typing += k

    # -- what is on screen ---------------------------------------------------

    def lines(self, width: int) -> list[tuple[str, bool]]:
        """The visible rows as text, and which one has the cursor.

        A tag goes in a column of its own after the first cell, so a long row
        cannot push it off the screen.
        """
        tagged = any(r.tag for r in self.rows)
        table = [[r.cells[0], r.tag, *r.cells[1:]] if tagged else r.cells
                 for r in self.rows]
        widths = [0] * max((len(cells) for cells in table), default=0)
        for cells in table:
            for i, c in enumerate(cells):
                widths[i] = max(widths[i], len(c))
        out = []
        for i in range(self.top, min(self.top + self.height, len(self.rows))):
            text = "  ".join(c.ljust(widths[n]) for n, c in enumerate(table[i]))
            mark = "> " if i == self.cursor else "  "
            out.append(((mark + text.rstrip())[:width - 1], i == self.cursor))
        return out

    def footer(self) -> str:
        if self.typing is not None:
            return f"/{self.typing}"
        if self.message:
            return self.message
        typing = "   / type one" if self.typed is not None else ""
        return f"j/k move   enter choose{typing}   q cancel"


def choose(header: list[str], rows: list[Row], cursor: int = 0,
           typed: Typed | None = None) -> str:
    """Draw the list, and return what was chosen. Refused when cancelled."""
    picker = Picker(rows, cursor=cursor, typed=typed)
    os.environ.setdefault("ESCDELAY", "25")     # Esc should not wait a second

    def draw(screen: "curses.window") -> None:
        curses.curs_set(0)
        while not picker.done:
            h, w = screen.getmaxyx()
            picker.height = max(1, h - len(header) - 2)
            picker.scroll()
            screen.erase()
            y = 0
            for line in header:
                screen.addnstr(y, 0, line, w - 1)
                y += 1
            for text, here in picker.lines(w):
                screen.addnstr(y, 0, text, w - 1,
                               curses.A_REVERSE if here else curses.A_NORMAL)
                y += 1
            screen.addnstr(min(y + 1, h - 1), 0, picker.footer(), w - 1,
                           curses.A_DIM)
            screen.refresh()
            try:
                k = screen.get_wch()
            except KeyboardInterrupt:
                k = "\x03"
            picker.key(curses.keyname(k).decode() if isinstance(k, int) else k)

    curses.wrapper(draw)
    if picker.chosen is None:
        raise Refused("cancelled; nothing was created")
    return picker.chosen
