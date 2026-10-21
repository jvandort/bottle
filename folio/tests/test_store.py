import os
import tempfile
import unittest
from pathlib import Path

from folio import store


class SlotTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name) / "thread"

    def read(self, name: str) -> bytes:
        return (store.current_slot(self.dir) / name).read_bytes()

    def slots(self) -> list[str]:
        return sorted(p.name for p in self.dir.iterdir() if p.is_dir() and not p.is_symlink())

    def test_nothing_written_is_no_slot(self) -> None:
        self.assertIsNone(store.current_slot(self.dir))

    def test_a_write_becomes_current(self) -> None:
        slot = store.write_slot(self.dir, {"a": b"1"})
        self.assertEqual(store.current_slot(self.dir), slot)
        self.assertEqual(self.read("a"), b"1")
        self.assertTrue((self.dir / store.CURRENT).is_symlink())

    def test_files_not_written_carry_over_as_links(self) -> None:
        first = store.write_slot(self.dir, {"a": b"1", "big": b"archive"})
        inode = (first / "big").stat().st_ino
        second = store.write_slot(self.dir, {"a": b"2"})
        self.assertEqual((self.read("a"), self.read("big")), (b"2", b"archive"))
        self.assertEqual((second / "big").stat().st_ino, inode)  # linked, not copied

    def test_the_old_slot_goes(self) -> None:
        store.write_slot(self.dir, {"a": b"1"})
        second = store.write_slot(self.dir, {"a": b"2"})
        self.assertEqual(self.slots(), [second.name])

    def test_a_crash_leaves_current_and_the_next_write_clears_up(self) -> None:
        first = store.write_slot(self.dir, {"a": b"1"})
        (self.dir / "20990101T000000.000Z").mkdir()  # a slot whose write never finished
        self.assertEqual(self.read("a"), b"1")
        second = store.write_slot(self.dir, {"a": b"2"})
        self.assertEqual(self.slots(), [second.name])
        self.assertFalse(first.exists())


class JsonTest(unittest.TestCase):
    def test_round_trip_and_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "deep" / "x.json"
            self.assertIsNone(store.read_json(path))
            store.write_json(path, {"a": 1})
            self.assertEqual(store.read_json(path), {"a": 1})
            self.assertEqual([p.name for p in path.parent.iterdir()], ["x.json"])  # no staging left behind
