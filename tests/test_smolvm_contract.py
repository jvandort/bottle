"""What bottle relies on smolvm for (see smolvm.py's docstring), against a real smolvm.

Runs only where smolvm is: a Mac with it on PATH, or $BOTTLE_SMOLVM naming
one. Checks smolvm through bottle's own smolvm module, with a stand-in for
bottled's decider, on one alpine machine made for the module and deleted
after. The first run pulls alpine and buildah's image, which takes a while.
Run it after upgrading smolvm.
"""

import fcntl
import hashlib
import os
import pty
import secrets
import select
import shutil
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

from bottle import daemon, decider, host, images, smolvm
from bottle.errors import BottleError
from tests.support import smolvm_present

STANDIN = "the-egress-proxy-holds-the-real-one"
# An address the stand-in decider denies, to see what a denied flow does in the guest.
DENIED = "192.0.2.99"


class StandInDecider:
    """bottled's decider, as little as smolvm needs.

    Every flow goes direct, except bottle.host on port 80, which is redirected
    and answered here as bottled answers git, and DENIED, which is denied.
    Records each flow it's asked about.
    """

    def __init__(self, token: bytes) -> None:
        self.token = token
        self.flows: list[tuple[str, int, bytes, str]] = []  # address, port, first bytes, answer
        self.server = socket.create_server(("127.0.0.1", 0))
        self.address = f"127.0.0.1:{self.server.getsockname()[1]}"
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
            head = stream.read(75)
            if head[:8] != decider.MAGIC or head[8:40] != self.token:
                return
            family, port = head[72], int.from_bytes(head[73:75], "big")
            packed = stream.read(4 if family == 4 else 16)
            address = socket.inet_ntop(socket.AF_INET if family == 4 else socket.AF_INET6, packed)
            first = stream.read(int.from_bytes(stream.read(2), "big"))
            if (address, port) == (host.ADDRESS, 80):
                self.flows.append((address, port, first, "redirect"))
                body = b"answered by the decider\n"
                connection.sendall(
                    decider.REDIRECT
                    + b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body) + body)
            elif address == DENIED:
                self.flows.append((address, port, first, "deny"))
                connection.sendall(decider.DENY)
            else:
                self.flows.append((address, port, first, "direct"))
                connection.sendall(decider.ALLOW_DIRECT)

    def close(self) -> None:
        self.server.close()


# Made by setUpModule, for every test here.
machine = ""
stand_in: StandInDecider = None  # type: ignore[assignment]
token = ""
work = Path()


def setUpModule() -> None:
    global machine, stand_in, token, work
    if not smolvm_present():
        raise unittest.SkipTest("needs smolvm ($BOTTLE_SMOLVM, or smolvm on PATH)")
    work = Path(tempfile.mkdtemp(prefix="smolvm-contract-"))
    unittest.addModuleCleanup(shutil.rmtree, work, True)
    # Builds write their image store here, not into the user's own.
    home = mock.patch.dict(os.environ, {"BOTTLE_HOME": str(work / "home")})
    home.start()
    unittest.addModuleCleanup(home.stop)
    (work / "ro").mkdir()
    (work / "ro" / "file").write_text("from the host\n")
    machine = f"contract-{uuid.uuid4().hex[:8]}"
    secret = secrets.token_bytes(32)
    token = secret.hex()
    stand_in = StandInDecider(secret)
    unittest.addModuleCleanup(stand_in.close)
    smolvm.create(machine, "alpine", mounts=[smolvm.Mount(work / "ro", "/mnt/ro")],
                  env={"STANDIN": STANDIN}, memory_mib=None, cpus=2)
    unittest.addModuleCleanup(smolvm.delete, machine)
    smolvm.start(machine, stand_in.address, token)
    smolvm.exec_(machine, ["sh", "-c", f"echo '{host.ADDRESS} {host.HOST}' >> /etc/hosts"], user="root")


def run(argv: list[str], input: bytes | None = None, **kwargs) -> subprocess.CompletedProcess:
    """`argv` in the machine as bottle runs a command for a caller (bottle exec), bytes in and out."""
    return subprocess.run(smolvm.exec_command(machine, argv, **kwargs), input=input, capture_output=True)


class MediatedEgressTest(unittest.TestCase):
    def test_bottle_host_is_redirected_and_answered(self) -> None:
        answer = smolvm.exec_(machine, ["wget", "-qO-", "-T", "5", "http://bottle.host/git/x"])
        self.assertEqual(answer, "answered by the decider\n")
        [(_, _, first, _)] = [f for f in stand_in.flows if f[3] == "redirect" and b"/git/x" in f[2]]
        # The decider gets the request's first bytes, Host included: how it names the flow.
        self.assertTrue(first.startswith(b"GET /git/x "), first)
        self.assertIn(b"Host: bottle.host", first)

    def test_other_flows_reach_the_decider_and_go_direct(self) -> None:
        smolvm.exec_(machine, ["wget", "-q", "-T", "10", "-O", "/dev/null", "https://example.com"])
        tls = [f for f in stand_in.flows if f[1] == 443 and f[3] == "direct"]
        self.assertTrue(tls)
        # A ClientHello, which is where bottled reads a credential's host from.
        self.assertTrue(any(first[:1] == b"\x16" for _, _, first, _ in tls))

    def test_a_denied_flow_fails_at_once(self) -> None:
        started = time.monotonic()
        result = run(["wget", "-q", "-T", "10", "-O", "/dev/null", f"http://{DENIED}/"])
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(time.monotonic() - started, 5)  # reset, not left to time out
        self.assertIn((DENIED, 80), [(a, p) for a, p, _, answer in stand_in.flows if answer == "deny"])


class ExecTest(unittest.TestCase):
    def test_as_a_user_in_a_directory(self) -> None:
        self.assertEqual(smolvm.exec_(machine, ["id", "-un"], user="root").strip(), "root")
        self.assertEqual(smolvm.exec_(machine, ["sh", "-c", 'echo "$(id -un) $(pwd)"'], user="nobody", workdir="/tmp"),
                         "nobody /tmp\n")

    def test_the_machines_environment_reaches_every_user(self) -> None:
        for user in (None, "nobody"):
            with self.subTest(user=user):
                self.assertEqual(smolvm.exec_(machine, ["sh", "-c", 'echo "$STANDIN"'], user=user), STANDIN + "\n")
        self.assertEqual(smolvm.exec_(machine, ["sh", "-c", 'echo "$STANDIN"'], env={"STANDIN": "override"}),
                         "override\n")

    def test_the_commands_status_comes_back(self) -> None:
        for status in (0, 1, 7):
            with self.subTest(status=status):
                self.assertEqual(run(["sh", "-c", f"exit {status}"]).returncode, status)

    def test_smolvms_own_failures_are_bottle_errors(self) -> None:
        self.assertIsNone(smolvm.state(f"no-such-{machine}"))
        with self.assertRaises(BottleError):
            smolvm.exec_(f"no-such-{machine}", ["true"])


class BytesTest(unittest.TestCase):
    """Snapshots and builds' output go through exec, so it carries bytes whole, both ways."""

    def test_50_mb_in_through_stdin(self) -> None:
        blob = os.urandom(50_000_000)
        result = run(["sha256sum"], input=blob)
        self.assertEqual(result.stdout.split()[0].decode(), hashlib.sha256(blob).hexdigest())

    def test_50_mb_out_through_stdout(self) -> None:
        # A plain smolvm exec cuts output at 11 MiB without a word; bottle's always passes -i.
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
                argv = smolvm.exec_command(machine, ["sh", "-c", "stty size; sleep 4; stty size"], tty=True)
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


class MountTest(unittest.TestCase):
    def test_a_read_only_mount_reads_and_refuses_writes(self) -> None:
        self.assertEqual(smolvm.exec_(machine, ["cat", "/mnt/ro/file"]), "from the host\n")
        self.assertNotEqual(run(["sh", "-c", "echo x > /mnt/ro/new"]).returncode, 0)
        self.assertFalse((work / "ro" / "new").exists())


class RestartTest(unittest.TestCase):
    def test_stop_and_start_keep_the_disk(self) -> None:
        smolvm.exec_(machine, ["sh", "-c", "echo still here > /root/marker"])
        smolvm.stop(machine)
        self.assertNotEqual(smolvm.state(machine), "running")
        smolvm.start(machine, stand_in.address, token)
        self.assertEqual(smolvm.state(machine), "running")
        self.assertEqual(smolvm.exec_(machine, ["cat", "/root/marker"]), "still here\n")


class BuildTest(unittest.TestCase):
    """Bottle builds images on smolvm's machines (images._build): buildah in a machine from a registry image,
    its inputs in and the archive out over exec; then a machine boots the archive."""

    def test_a_built_image_boots(self) -> None:
        contexts = work / "contexts"
        (contexts / "probe").mkdir(parents=True)
        (contexts / "probe" / "Dockerfile").write_text("FROM alpine\nRUN echo built in smolvm > /hello\n")
        # The builder bound to the stand-in rather than to bottled.
        with mock.patch.object(daemon, "bind_decider", return_value=(stand_in.address, token)), \
                mock.patch.object(daemon, "release_egress"):
            archive = images._build(contexts / "probe", "localhost/bottle/probe:latest",
                                    {"bottle.inputs": "probe"})
        self.assertEqual(images.archive_of("localhost/bottle/probe:latest"), archive)
        built = f"{machine}-built"
        smolvm.create(built, str(archive), mounts=[], env={}, memory_mib=None, cpus=1)
        self.addCleanup(smolvm.delete, built)
        smolvm.start(built, stand_in.address, token)
        self.assertEqual(smolvm.exec_(built, ["cat", "/hello"]), "built in smolvm\n")


if __name__ == "__main__":
    unittest.main()
