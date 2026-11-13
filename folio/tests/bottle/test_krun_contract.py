"""What bottle relies on bottle-cli for (see krun.py's docstring), against a real bottle-cli.

Runs only where bottle-cli can run VMs: a Mac, with bottle-cli built (`cargo
xtask dist`) or $BOTTLE_CLI naming one. Checks it through bottle's own krun
module, with a stand-in for bottled's services socket, on one alpine machine made for
the module and deleted after, its BOTTLE_HOME a temporary directory. Run it
after changing bottle-cli, the agent or libkrun.
"""

import fcntl
import hashlib
import os
import pty
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time
import unittest
import uuid
from pathlib import Path
from unittest import mock

from folio.bottle import daemon, host, images, krun, services
from folio.bottle.errors import BottleError
from tests.bottle.support import krun_present

STANDIN = "the-egress-proxy-holds-the-real-one"
# An address the machine's policy refuses (private, no name allowed there), to see what a refused flow does.
DENIED = "192.0.2.99"


class StandInService:
    """bottled's services socket, as little as bottle-cli needs: what the machine's policy routes here (bottle.host
    on port 80) is answered as bottled answers git. Records each connection: what its PROXY header said, and its
    first bytes."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.flows: list[tuple[str, int, str | None, str | None, bytes]] = []  # address, port, name, machine, first
        self.server = socket.socket(socket.AF_UNIX)
        self.server.bind(str(path))
        self.server.listen()
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self) -> None:
        while True:
            try:
                connection, _ = self.server.accept()
            except OSError:
                return
            threading.Thread(target=self._answer, args=(connection,), daemon=True).start()

    def _answer(self, connection: socket.socket) -> None:
        with connection, connection.makefile("rb") as stream:
            fixed = stream.read(16)
            if fixed[:12] != services.SIGNATURE:
                return
            body = stream.read(int.from_bytes(fixed[14:16], "big"))
            address, port = socket.inet_ntoa(body[4:8]), int.from_bytes(body[10:12], "big")
            fields, at = {}, 12
            while at < len(body):
                length = int.from_bytes(body[at + 1:at + 3], "big")
                fields[body[at]] = body[at + 3:at + 3 + length].decode(errors="replace")
                at += 3 + length
            first = stream.readline()
            self.flows.append((address, port, fields.get(services.PP2_TYPE_AUTHORITY),
                               fields.get(services.PP2_TYPE_MACHINE), first))
            body_out = b"answered by the service\n"
            connection.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body_out)
                               + body_out)

    def close(self) -> None:
        self.server.close()


# Made by setUpModule, for every test here.
machine = ""
stand_in: StandInService = None  # type: ignore[assignment]
work = Path()


def setUpModule() -> None:
    global machine, stand_in, work
    if not krun_present():
        raise unittest.SkipTest("needs bottle-cli on a Mac (cargo xtask dist, or $BOTTLE_CLI)")
    work = Path(tempfile.mkdtemp(prefix="krun-contract-"))
    unittest.addModuleCleanup(shutil.rmtree, work, True)
    # Images and machines are kept here, not in the user's own.
    home = mock.patch.dict(os.environ, {"BOTTLE_HOME": str(work / "home")})
    home.start()
    unittest.addModuleCleanup(home.stop)
    (work / "ro").mkdir()
    (work / "ro" / "file").write_text("from the host\n")
    machine = f"contract-{uuid.uuid4().hex[:8]}"
    stand_in = StandInService(work / "services.sock")
    unittest.addModuleCleanup(stand_in.close)
    alpine = subprocess.run([krun.binary(), "image", "pull", "alpine"], capture_output=True, text=True,
                            check=True).stdout.split()[-1]
    krun.create(machine, alpine, mounts=[krun.Mount(work / "ro", "/mnt/ro")],
                env={"STANDIN": STANDIN}, memory_mib=None, cpus=2,
                network=services.network([], [], sockets=dict.fromkeys(daemon.SERVICES, str(stand_in.path))))
    unittest.addModuleCleanup(krun.delete, machine)
    krun.start(machine)
    krun.exec_(machine, ["sh", "-c", f"echo '{host.ADDRESS} {host.HOST}' >> /etc/hosts"], user="root")


def run(argv: list[str], input: bytes | None = None, **kwargs) -> subprocess.CompletedProcess:
    """`argv` in the machine as bottle runs a command for a caller (bottle exec), bytes in and out."""
    return subprocess.run(krun.exec_command(machine, argv, **kwargs), input=input, capture_output=True)


class DecidedEgressTest(unittest.TestCase):
    def test_bottle_host_is_routed_to_bottleds_socket_and_answered(self) -> None:
        answer = krun.exec_(machine, ["wget", "-qO-", "-T", "5", "http://bottle.host/git/x"])
        self.assertEqual(answer, "answered by the service\n")
        [(address, port, name, said_machine, first)] = [f for f in stand_in.flows if b"/git/x" in f[4]]
        # Where the bottle connected, the name it asked for, and the machine: what bottle-cli says first.
        self.assertEqual((address, port, name, said_machine), (host.ADDRESS, 80, host.HOST, machine))
        self.assertTrue(first.startswith(b"GET /git/x "), first)

    def test_public_flows_go_direct(self) -> None:
        krun.exec_(machine, ["wget", "-q", "-T", "10", "-O", "/dev/null", "https://example.com"])
        self.assertFalse([f for f in stand_in.flows if f[1] == 443])  # nothing of bottled's

    def test_a_denied_flow_fails_at_once(self) -> None:
        started = time.monotonic()
        result = run(["wget", "-q", "-T", "10", "-O", "/dev/null", f"http://{DENIED}/"])
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(time.monotonic() - started, 5)  # reset, not left to time out


class ExecTest(unittest.TestCase):
    def test_as_a_user_in_a_directory(self) -> None:
        self.assertEqual(krun.exec_(machine, ["id", "-un"], user="root").strip(), "root")
        self.assertEqual(krun.exec_(machine, ["sh", "-c", 'echo "$(id -un) $(pwd)"'], user="nobody", workdir="/tmp"),
                         "nobody /tmp\n")

    def test_the_machines_environment_reaches_every_user(self) -> None:
        for user in (None, "nobody"):
            with self.subTest(user=user):
                self.assertEqual(krun.exec_(machine, ["sh", "-c", 'echo "$STANDIN"'], user=user), STANDIN + "\n")
        self.assertEqual(krun.exec_(machine, ["sh", "-c", 'echo "$STANDIN"'], env={"STANDIN": "override"}),
                         "override\n")

    def test_the_commands_status_comes_back(self) -> None:
        for status in (0, 1, 7):
            with self.subTest(status=status):
                self.assertEqual(run(["sh", "-c", f"exit {status}"]).returncode, status)

    def test_bottle_clis_own_failures_are_bottle_errors(self) -> None:
        self.assertIsNone(krun.state(f"no-such-{machine}"))
        with self.assertRaises(BottleError):
            krun.exec_(f"no-such-{machine}", ["true"])


class BytesTest(unittest.TestCase):
    """Snapshots and builds' output go through exec, so it carries bytes whole, both ways."""

    def test_50_mb_in_through_stdin(self) -> None:
        blob = os.urandom(50_000_000)
        result = run(["sha256sum"], input=blob)
        self.assertEqual(result.stdout.split()[0].decode(), hashlib.sha256(blob).hexdigest())

    def test_50_mb_out_through_stdout(self) -> None:
        result = run(["head", "-c", "50000000", "/dev/zero"])
        self.assertEqual((result.returncode, len(result.stdout)), (0, 50_000_000))

    def test_stdins_end_reaches_the_command(self) -> None:
        self.assertEqual(run(["wc", "-c"], input=b"abc").stdout.strip(), b"3")


class TtyTest(unittest.TestCase):
    def test_the_tty_has_the_terminals_size_and_follows_it(self) -> None:
        pid, terminal = pty.fork()
        if pid == 0:  # the child, on the pty: never back into the test runner
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))
                argv = krun.exec_command(machine, ["sh", "-c", "stty size; sleep 4; stty size"], tty=True)
                os.execvp(argv[0], argv)
            finally:
                os._exit(127)
        output = b""
        resized = False
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            ready, _, _ = select.select([terminal], [], [], 1)
            if ready:
                try:
                    chunk = os.read(terminal, 1024)
                except OSError:  # the command ended
                    break
                if not chunk:
                    break
                output += chunk
            if not resized and b"40 100" in output:
                fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 90, 0, 0))
                resized = True
        os.waitpid(pid, 0)
        self.assertEqual(output.replace(b"\r", b"").split(b"\n")[:2], [b"40 100", b"30 90"])

    def test_ctrl_c_typed_interrupts_the_command(self) -> None:
        pid, terminal = pty.fork()
        if pid == 0:  # the child, on the pty: never back into the test runner
            try:
                script = "trap 'echo interrupted; exit 3' INT; echo ready; while :; do sleep 0.1; done"
                argv = krun.exec_command(machine, ["sh", "-c", script], tty=True)
                os.execvp(argv[0], argv)
            finally:
                os._exit(127)
        output = b""
        typed = False
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and b"interrupted" not in output:
            ready, _, _ = select.select([terminal], [], [], 1)
            if ready:
                try:
                    chunk = os.read(terminal, 1024)
                except OSError:  # the command ended
                    break
                if not chunk:
                    break
                output += chunk
            if not typed and b"ready" in output:
                os.write(terminal, b"\x03")  # raw here: the terminal in the machine makes it SIGINT
                typed = True
        if b"interrupted" not in output:
            os.kill(pid, signal.SIGTERM)
        _, status = os.waitpid(pid, 0)
        self.assertIn(b"interrupted", output)
        self.assertEqual(os.waitstatus_to_exitcode(status), 3)


class MountTest(unittest.TestCase):
    def test_a_read_only_mount_reads_and_refuses_writes(self) -> None:
        self.assertEqual(krun.exec_(machine, ["cat", "/mnt/ro/file"]), "from the host\n")
        self.assertNotEqual(run(["sh", "-c", "echo x > /mnt/ro/new"]).returncode, 0)
        self.assertFalse((work / "ro" / "new").exists())


class RestartTest(unittest.TestCase):
    def test_stop_and_start_keep_the_disk(self) -> None:
        krun.exec_(machine, ["sh", "-c", "echo still here > /root/marker"])
        krun.stop(machine)
        self.assertNotEqual(krun.state(machine), "running")
        krun.start(machine)
        self.assertEqual(krun.state(machine), "running")
        self.assertEqual(krun.exec_(machine, ["cat", "/root/marker"]), "still here\n")


class BuildTest(unittest.TestCase):
    """Bottle builds images with bottle-cli (images.build_recipe): a recipe's steps in a VM on a registry's image,
    its directory mounted where they run, its connections decided; then a machine boots the stage it made, with
    the environment the recipe set."""

    def test_a_built_image_boots(self) -> None:
        context = work / "probe"
        context.mkdir()
        (context / "install.sh").write_text("#!/bin/sh\nset -e\nwget -q -T 10 -O /dev/null https://example.com\n"
                                            "cat message > /hello\n")
        (context / "message").write_text("built by bottle-cli\n")
        recipe = {"pull": "alpine", "steps": [{"env": {"PROBE": "set by the recipe"}},
                                              {"run": images.interpreter(context / "install.sh"), "cwd": images.MOUNT}]}
        disk = images.build_recipe(context, "localhost/bottle/probe:latest", "probe", recipe)
        self.assertIsNotNone(images.built_labels("localhost/bottle/probe:latest"))
        built = f"{machine}-built"
        krun.create(built, str(disk), mounts=[], env={}, memory_mib=None, cpus=1,
                    network=services.network([], [], sockets=dict.fromkeys(daemon.SERVICES, str(stand_in.path))))
        self.addCleanup(krun.delete, built)
        krun.start(built)
        self.assertEqual(krun.exec_(built, ["cat", "/hello"]), "built by bottle-cli\n")
        self.assertEqual(krun.exec_(built, ["sh", "-c", 'echo "$PROBE"']), "set by the recipe\n")


if __name__ == "__main__":
    unittest.main()
