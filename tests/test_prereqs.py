import contextlib
import io
import subprocess
import unittest
from unittest import mock

from bottle import prereqs
from bottle.errors import BottleError


class FakeHost:
    """A Mac whose installed tools are configurable."""

    def __init__(self, tools=("brew", "smolvm"), machine="arm64", tty=True, answer="y"):
        self.tools = set(tools)
        self.machine = machine
        self.tty = tty
        self.answer = answer
        self.commands: list[list[str]] = []
        self.prompts: list[str] = []

    def run(self, cmd, capture_output=False, text=False):
        self.commands.append(cmd)
        if cmd == ["brew", "install", "smol-machines/tap/smolvm"]:
            self.tools.add("smolvm")
        return subprocess.CompletedProcess(cmd, 0)

    def input(self, prompt):
        self.prompts.append(prompt)
        return self.answer

    @contextlib.contextmanager
    def active(self):
        with (
            mock.patch.object(prereqs.platform, "system", return_value="Darwin"),
            mock.patch.object(prereqs.platform, "machine", return_value=self.machine),
            mock.patch.object(prereqs.shutil, "which", side_effect=lambda t: f"/bin/{t}" if t in self.tools else None),
            mock.patch.object(prereqs.subprocess, "run", side_effect=self.run),
            mock.patch.object(prereqs.sys.stdin, "isatty", return_value=self.tty),
            mock.patch("builtins.input", side_effect=self.input),
            contextlib.redirect_stderr(io.StringIO()) as stderr,
        ):
            self.stderr = stderr
            yield self


class EnsureSmolvmTest(unittest.TestCase):
    def setUp(self) -> None:
        prereqs.ensure_smolvm.cache_clear()
        self.addCleanup(prereqs.ensure_smolvm.cache_clear)

    def test_silent_when_ready(self) -> None:
        with FakeHost().active() as host:
            prereqs.ensure_smolvm()
        self.assertEqual((host.commands, host.prompts, host.stderr.getvalue()), ([], [], ""))

    def test_installs_smolvm_after_prompt(self) -> None:
        with FakeHost(tools=("brew",)).active() as host:
            prereqs.ensure_smolvm()
        self.assertEqual(host.commands, [["brew", "install", "smol-machines/tap/smolvm"]])
        self.assertEqual(len(host.prompts), 1)

    def test_declined_install_fails(self) -> None:
        with FakeHost(tools=("brew",), answer="n").active() as host, self.assertRaises(BottleError):
            prereqs.ensure_smolvm()
        self.assertEqual(host.commands, [])

    def test_never_installs_without_a_terminal(self) -> None:
        with FakeHost(tools=("brew",), tty=False).active() as host, \
                self.assertRaisesRegex(BottleError, "brew install smol-machines/tap/smolvm"):
            prereqs.ensure_smolvm()
        self.assertEqual((host.commands, host.prompts), ([], []))

    def test_requires_homebrew(self) -> None:
        with FakeHost(tools=()).active(), self.assertRaisesRegex(BottleError, "Homebrew"):
            prereqs.ensure_smolvm()

    def test_checks_once_per_process(self) -> None:
        with FakeHost().active():
            prereqs.ensure_smolvm()
        with FakeHost(tools=()).active():
            prereqs.ensure_smolvm()  # would fail if it checked again

    def test_rejects_intel(self) -> None:
        with FakeHost(machine="x86_64").active(), self.assertRaisesRegex(BottleError, "Apple silicon"):
            prereqs.ensure_smolvm()
