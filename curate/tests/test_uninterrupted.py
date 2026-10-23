"""Ctrl-C held off while refs move one at a time."""

import os
import signal
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from curate.ui import uninterrupted


class Uninterrupted(unittest.TestCase):
    def test_ctrl_c_inside_is_ignored(self) -> None:
        with uninterrupted():
            os.kill(os.getpid(), signal.SIGINT)
        # Reaching here is the test: a KeyboardInterrupt would have ended it.

    def test_and_afterwards_it_stops_things_again(self) -> None:
        before = signal.getsignal(signal.SIGINT)
        with uninterrupted():
            with uninterrupted():
                pass
            self.assertIs(signal.getsignal(signal.SIGINT), signal.SIG_IGN)
        self.assertIs(signal.getsignal(signal.SIGINT), before)
        with self.assertRaises(KeyboardInterrupt):
            os.kill(os.getpid(), signal.SIGINT)
