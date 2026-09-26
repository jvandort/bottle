import contextlib
import io
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import prereqs
from bottle.errors import BottleError

RUNNING = "FIELD   VALUE\nstatus  running\n"
STOPPED = "FIELD   VALUE\nstatus  stopped\n"


class FakeHost:
    """A macOS host whose installed tools, service state and kernel are configurable."""

    def __init__(self, tmp: Path, tools=("brew", "container"), status=RUNNING, kernel=True, tty=True, answer="y"):
        self.tools = set(tools)
        self.status = status
        self.kernel = tmp / "default.kernel-arm64"
        if kernel:
            self.kernel.touch()
        self.tty = tty
        self.answer = answer
        self.commands: list[list[str]] = []
        self.prompts: list[str] = []

    def run(self, cmd, capture_output=False, text=False):
        if cmd == ["container", "system", "status"]:
            return subprocess.CompletedProcess(cmd, 0, stdout=self.status, stderr="")
        self.commands.append(cmd)
        if cmd == ["brew", "install", "container"]:
            self.tools.add("container")
        elif cmd[:3] == ["container", "system", "start"]:
            self.status = RUNNING
        elif cmd[:4] == ["container", "system", "kernel", "set"]:
            self.kernel.touch()
        return subprocess.CompletedProcess(cmd, 0)

    def input(self, prompt):
        self.prompts.append(prompt)
        return self.answer

    @contextlib.contextmanager
    def active(self):
        with (
            mock.patch.object(prereqs.platform, "system", return_value="Darwin"),
            mock.patch.object(prereqs.platform, "machine", return_value="arm64"),
            mock.patch.object(prereqs.platform, "mac_ver", return_value=("26.7", ("", "", ""), "arm64")),
            mock.patch.object(prereqs.shutil, "which", side_effect=lambda t: f"/bin/{t}" if t in self.tools else None),
            mock.patch.object(prereqs.subprocess, "run", side_effect=self.run),
            mock.patch.object(prereqs, "KERNEL", self.kernel),
            mock.patch.object(prereqs.sys.stdin, "isatty", return_value=self.tty),
            mock.patch("builtins.input", side_effect=self.input),
            contextlib.redirect_stderr(io.StringIO()) as stderr,
        ):
            self.stderr = stderr
            yield self


class EnsureContainerTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        prereqs.ensure_container.cache_clear()
        self.addCleanup(prereqs.ensure_container.cache_clear)

    def host(self, **kwargs) -> FakeHost:
        return FakeHost(self.tmp, **kwargs)

    def test_silent_when_ready(self) -> None:
        with self.host().active() as host:
            prereqs.ensure_container()
        self.assertEqual(host.commands, [])
        self.assertEqual(host.prompts, [])
        self.assertEqual(host.stderr.getvalue(), "")

    def test_installs_container_after_prompt(self) -> None:
        with self.host(tools={"brew"}).active() as host:
            prereqs.ensure_container()
        self.assertEqual(host.commands, [["brew", "install", "container"]])
        self.assertEqual(len(host.prompts), 1)

    def test_declined_install_fails(self) -> None:
        with self.host(tools={"brew"}, answer="n").active() as host, self.assertRaisesRegex(BottleError, "required"):
            prereqs.ensure_container()
        self.assertEqual(host.commands, [])

    def test_never_installs_without_a_terminal(self) -> None:
        with self.host(tools={"brew"}, tty=False).active() as host, self.assertRaisesRegex(BottleError, "brew install container"):
            prereqs.ensure_container()
        self.assertEqual(host.commands, [])
        self.assertEqual(host.prompts, [])

    def test_requires_homebrew(self) -> None:
        with self.host(tools=()).active(), self.assertRaisesRegex(BottleError, "Homebrew"):
            prereqs.ensure_container()

    def test_starts_services_without_prompt(self) -> None:
        with self.host(status=STOPPED).active() as host:
            prereqs.ensure_container()
        self.assertEqual(host.commands, [["container", "system", "start", "--disable-kernel-install"]])
        self.assertEqual(host.prompts, [])

    def test_installs_kernel_after_prompt(self) -> None:
        with self.host(kernel=False).active() as host:
            prereqs.ensure_container()
        self.assertEqual(host.commands, [["container", "system", "kernel", "set", "--recommended"]])
        self.assertEqual(len(host.prompts), 1)

    def test_fresh_host(self) -> None:
        with self.host(tools={"brew"}, status=STOPPED, kernel=False).active() as host:
            prereqs.ensure_container()
        self.assertEqual(
            host.commands,
            [
                ["brew", "install", "container"],
                ["container", "system", "start", "--disable-kernel-install"],
                ["container", "system", "kernel", "set", "--recommended"],
            ],
        )
        self.assertEqual(len(host.prompts), 2)

    def test_checks_once_per_process(self) -> None:
        with self.host().active():
            prereqs.ensure_container()
        with self.host(tools=()).active():
            prereqs.ensure_container()  # would fail if it checked again

    def test_rejects_old_macos(self) -> None:
        with (
            self.host().active(),
            mock.patch.object(prereqs.platform, "mac_ver", return_value=("15.4", ("", "", ""), "arm64")),
            self.assertRaisesRegex(BottleError, "macOS 26"),
        ):
            prereqs.ensure_container()

    def test_rejects_intel(self) -> None:
        with (
            self.host().active(),
            mock.patch.object(prereqs.platform, "machine", return_value="x86_64"),
            self.assertRaisesRegex(BottleError, "Apple silicon"),
        ):
            prereqs.ensure_container()


if __name__ == "__main__":
    unittest.main()
