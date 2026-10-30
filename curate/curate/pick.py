"""Choosing from a list, on a terminal: one row, or a checkbox on each.

Two halves. `Picker` is what the list is and what a key does to it, and knows
nothing about terminals, so tests drive it directly.

`/` searches, as in vim: the list narrows to the rows with what you type in
them, and enter stops typing and keeps it narrowed, for j and k and enter as
ever; esc brings every row back. A list that takes text too (`typed`) takes
what you typed when no row matches: so `/main~1` is a revision. `choose` and
`choose_many` draw it with curses and feed it keys.
"""

import curses
import os
from dataclasses import dataclass
from typing import Callable

from .errors import Refused
from .timing import waiting

# What `typed` answers: the value, or why not.
Typed = Callable[[str], tuple[str | None, str]]


@dataclass
class Row:
    cells: list[str]                 # columns, aligned across rows
    value: str | None = None         # what choosing it returns
    tag: str = ""                    # a label: "best guess"
    expand: Callable[[], list["Row"]] | None = None   # chosen, it becomes these
    prompt: str = ""                 # chosen, it asks for text after this
    checked: bool = False            # in a checkbox list


@dataclass
class Picker:
    rows: list[Row]
    cursor: int = 0
    top: int = 0                     # the first row on screen
    height: int = 10                 # rows on screen
    typing: str | None = None        # the text typed so far, while typing
    prompt: str = "/"                # what it is typed after
    message: str = ""                # one line under the list
    chosen: str | None = None
    cancelled: bool = False
    typed: Typed | None = None
    searching: str | None = None     # the search typed so far, while typing it
    query: str = ""                  # the search the list is narrowed to
    everything: list[Row] | None = None   # all the rows, while a search shows some
    # A checkbox on each row, and enter only from the last: getting there
    # means having passed every row, so every one is a decision made.
    checkboxes: bool = False

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

    @property
    def checked(self) -> list[str]:
        return [r.value for r in self.rows if r.checked and r.value is not None]

    def enter(self) -> None:
        row = self.rows[self.cursor]
        if self.checkboxes:
            if self.cursor == len(self.rows) - 1:
                self.chosen = ""
            else:
                self.message = "go through every row first: j to the last, then enter"
        elif row.prompt and self.typed is not None:
            self.typing, self.prompt, self.message = "", row.prompt, ""
        elif row.expand is not None:
            if self.everything is not None:     # its rows go in the whole list
                self.end_search()
            self.rows[self.cursor:self.cursor + 1] = row.expand()
            self.cursor = min(self.cursor, len(self.rows) - 1)
            self.scroll()
        elif row.value is not None:
            self.chosen = row.value

    def key(self, k: str) -> None:
        """One key: a character, or a curses name such as KEY_UP."""
        if self.searching is not None:
            self.search_key(k)
        elif self.typing is not None:
            self.type_key(k)
        elif k in ("j", "KEY_DOWN"):
            self.move(1)
        elif k in ("k", "KEY_UP"):
            self.move(-1)
        elif k == " " and self.checkboxes:
            row = self.rows[self.cursor]
            row.checked = not row.checked
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
        elif k == "\x1b" and self.everything is not None:
            self.end_search()
        elif k in ("q", "\x1b", "\x03", "\x04"):
            self.cancelled = True
        elif k == "/" and not self.checkboxes:
            if self.everything is None:
                self.everything = self.rows
            self.searching, self.message = self.query, ""

    # -- searching -----------------------------------------------------------

    def search_key(self, k: str) -> None:
        assert self.searching is not None
        if k in ("\n", "\r", "KEY_ENTER"):
            self.search_enter()
        elif k in ("\x1b", "\x03"):
            self.end_search()
        elif k in ("KEY_BACKSPACE", "\x7f", "\x08"):
            if not self.searching:
                self.end_search()
            else:
                self.narrow(self.searching[:-1])
        elif k == "KEY_DOWN":
            self.move(1)
        elif k == "KEY_UP":
            self.move(-1)
        elif k == "KEY_NPAGE":
            self.move(self.height)
        elif k == "KEY_PPAGE":
            self.move(-self.height)
        elif len(k) == 1 and k.isprintable():
            self.narrow(self.searching + k)

    def narrow(self, text: str) -> None:
        """The rows with `text` in them, any case; the cursor on one that's exactly it, else the first."""
        assert self.everything is not None
        self.searching = self.query = text
        needle = text.lower()
        self.rows = [r for r in self.everything
                     if needle in "  ".join([*r.cells, r.tag]).lower()]
        self.cursor = next((i for i, r in enumerate(self.rows) if r.cells[0].lower() == needle), 0)
        self.top = 0
        self.scroll()

    def end_search(self) -> None:
        """Back to every row, the cursor on the one it was on."""
        assert self.everything is not None
        here = self.rows[self.cursor] if self.rows else None
        self.rows, self.everything, self.searching, self.query = self.everything, None, None, ""
        self.cursor = next((i for i, r in enumerate(self.rows) if r is here), 0)
        self.scroll()

    def search_enter(self) -> None:
        """Stop typing, the list kept narrowed; with nothing typed, every row back. When nothing
        matches, a list that takes text takes it, or says why not."""
        text = self.searching or ""
        if not text:
            self.end_search()
        elif self.rows:
            self.searching = None
        elif self.typed is not None:
            value, why = self.typed(text)
            if value is not None:
                self.chosen = value
            else:
                self.message = why
                self.end_search()

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
            if self.checkboxes:
                mark += "[x] " if self.rows[i].checked else "[ ] "
            out.append(((mark + text.rstrip())[:width - 1], i == self.cursor))
        return out

    def footer(self) -> str:
        if self.searching is not None:
            if self.rows or not self.searching:
                return f"/{self.searching}"
            return f"/{self.searching}   no match" + ("; enter takes it as typed" if self.typed else "")
        if self.everything is not None and not self.message:
            return f"/{self.query}   j/k move   enter choose   / edit   esc all rows"
        if self.typing is not None:
            return f"{self.prompt}{self.typing}"
        if self.message:
            return self.message
        if self.checkboxes:
            return "j/k move   space toggles   enter on the last row confirms   q/esc cancel"
        return "j/k move   enter choose   / search   q/esc cancel"


def choose(header: list[str], rows: list[Row], cursor: int = 0,
           typed: Typed | None = None,
           cancelled: str = "cancelled; nothing was created") -> str:
    """Draw the list, and return what was chosen. Refused when cancelled."""
    picker = show(Picker(rows, cursor=cursor, typed=typed), header)
    if picker.chosen is None:
        raise Refused(cancelled)
    return picker.chosen


def choose_many(header: list[str], rows: list[Row],
                cancelled: str = "cancelled; nothing was changed") -> list[str]:
    """Checkboxes, confirmed from the last row. The values left checked."""
    picker = show(Picker(rows, checkboxes=True), header)
    if picker.chosen is None:
        raise Refused(cancelled)
    return picker.checked


def show(picker: Picker, header: list[str]) -> Picker:
    """Draw the list and feed it keys until it is done."""
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

    with waiting():
        curses.wrapper(draw)
    return picker
