"""That `curate --help` and the dispatch table still agree."""

import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import curate
from curate.cli import COMMANDS, HELP, INTERNAL, MODE_VERBS


class Help(unittest.TestCase):
    def test_every_verb_you_can_type_is_documented(self) -> None:
        for verb in sorted(set(COMMANDS) | set(MODE_VERBS)):
            with self.subTest(verb=verb):
                self.assertRegex(HELP, rf"(?m)^  curate {verb}\b")

    def test_and_nothing_is_documented_that_does_not_exist(self) -> None:
        listed = set(re.findall(r"^  curate (\S+)", HELP, re.M))
        self.assertEqual(listed - set(COMMANDS) - set(MODE_VERBS), set())

    def test_internal_verbs_are_not_advertised(self) -> None:
        """`hook` is git's to call, not yours."""
        for verb in INTERNAL:
            self.assertNotIn(f"curate {verb}", HELP)


class ModuleList(unittest.TestCase):
    """curate/__init__.py is the only place the seams are written down."""

    def modules(self) -> set[str]:
        package = Path(curate.__file__).parent
        return {p.stem for p in package.glob("*.py")} - {"__init__"}

    def test_every_module_is_described(self) -> None:
        doc = curate.__doc__ or ""
        for module in sorted(self.modules()):
            with self.subTest(module=module):
                self.assertRegex(doc, rf"(?m)^    {module}\b")

    def test_and_nothing_is_described_that_is_gone(self) -> None:
        doc = curate.__doc__ or ""
        described = set(re.findall(r"(?m)^    (\w+) {2,}", doc))
        self.assertEqual(described - self.modules(), set())
