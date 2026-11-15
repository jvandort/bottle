"""What folio relies on bottle-cli for (folio-cli's bottle_cli.rs says), against a real bottle-cli.

Runs only where bottle-cli can run VMs: a Mac, with bottle-cli built (`cargo xtask dist`: target/dist/bottle-cli)
or $BOTTLE_CLI naming one. Checks it as folio drives it, word for word, with a stand-in for folio serve's services
socket, on one alpine machine made for the module and deleted after, its BOTTLE_HOME a temporary directory. Run
it after changing bottle-cli, the agent or libkrun.
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
import json
import platform
import uuid
from pathlib import Path
from unittest import mock

BOTTLE_CLI = os.environ.get("BOTTLE_CLI") or str(Path(__file__).resolve().parents[2] / "target" / "dist" / "bottle-cli")
# Where a bottle reaches bottle.host, and folio serve's services, as folio's policy routes them.
HOST, HOST_ADDRESS, SERVICES = "bottle.host", "192.0.2.1", ("host", "ssh", "credentials")
# What bottle-cli says first on a routed connection: a PROXY protocol v2 header, and the fields in it of the name
# the bottle asked for and its machine.
SIGNATURE, PP2_TYPE_AUTHORITY, PP2_TYPE_MACHINE = b"\r\n\r\n\x00\r\nQUIT\n", 0x02, 0xE0
# Where a build's directory is in its VM.
MOUNT = "/run/bottle-build"

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
            if fixed[:12] != SIGNATURE:
                return
            body = stream.read(int.from_bytes(fixed[14:16], "big"))
            address, port = socket.inet_ntoa(body[4:8]), int.from_bytes(body[10:12], "big")
            fields, at = {}, 12
            while at < len(body):
                length = int.from_bytes(body[at + 1:at + 3], "big")
                fields[body[at]] = body[at + 3:at + 3 + length].decode(errors="replace")
                at += 3 + length
            first = stream.readline()
            self.flows.append((address, port, fields.get(PP2_TYPE_AUTHORITY), fields.get(PP2_TYPE_MACHINE), first))
            body_out = b"answered by the service\n"
            connection.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body_out)
                               + body_out)

    def close(self) -> None:
        self.server.close()


class Failed(Exception):
    """bottle-cli's own failure: its message."""


def cli(*args: str, input: str | None = None) -> str:
    """bottle-cli with `args`, its stdout; Failed, with its message, if it fails."""
    done = subprocess.run([BOTTLE_CLI, *args], input=input or "", capture_output=True, text=True)
    if done.returncode != 0:
        raise Failed(done.stderr.strip().splitlines()[-1].removeprefix("bottle-cli: ") if done.stderr.strip() else "")
    return done.stdout


def network(socket_path: Path) -> str:
    """A machine's policy, as folio makes one: bottle.host and ssh to bottles routed to `socket_path`, nothing cached."""
    return json.dumps({"allow_private": [], "cache": [], "routes": [
        {"address": HOST_ADDRESS, "port": 80, "socket": str(socket_path)},
        {"network": "192.0.2.0/24", "port": 22, "socket": str(socket_path)},
    ]})


def create(name: str, image: str, mounts: list[tuple[Path, str]], env: dict[str, str], cpus: int,
           policy: str) -> None:
    args = ["create", name, "--image", image, "--cpus", str(cpus)]
    for source, target in mounts:
        args += ["-v", f"{source}:{target}:ro"]
    for key, value in env.items():
        args += ["-e", f"{key}={value}"]
    cli(*args, "--network", "-", input=policy)


def delete(name: str) -> None:
    if state(name) is not None:
        cli("delete", name, "-f")


def state(name: str) -> str | None:
    try:
        return json.loads(cli("status", name, "--json"))["state"]
    except Failed as e:
        if str(e).startswith("no machine "):
            return None
        raise


def exec_args(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
              env: dict[str, str] | None = None, tty: bool = False) -> list[str]:
    args = ["exec", name, *(["-t"] if tty else [])]
    args += ["-u", user] if user else []
    args += ["-w", workdir] if workdir else []
    for key, value in (env or {}).items():
        args += ["-e", f"{key}={value}"]
    return [*args, "--", *argv]


def exec_(name: str, argv: list[str], user: str | None = None, workdir: str | None = None,
          env: dict[str, str] | None = None) -> str:
    return cli(*exec_args(name, argv, user, workdir, env))


def exec_command(name: str, argv: list[str], tty: bool = False) -> list[str]:
    return [BOTTLE_CLI, *exec_args(name, argv, tty=tty)]


# Made by setUpModule, for every test here.
machine = ""
stand_in: StandInService = None  # type: ignore[assignment]
work = Path()


def setUpModule() -> None:
    global machine, stand_in, work
    if platform.system() != "Darwin" or not os.path.isfile(BOTTLE_CLI):
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
    alpine = cli("image", "pull", "alpine").split()[-1]
    create(machine, alpine, [(work / "ro", "/mnt/ro")], {"STANDIN": STANDIN}, 2, network(stand_in.path))
    unittest.addModuleCleanup(delete, machine)
    cli("start", machine)
    exec_(machine, ["sh", "-c", f"echo '{HOST_ADDRESS} {HOST}' >> /etc/hosts"], user="root")


def run(argv: list[str], input: bytes | None = None, **kwargs) -> subprocess.CompletedProcess:
    """`argv` in the machine as bottle runs a command for a caller (bottle exec), bytes in and out."""
    return subprocess.run(exec_command(machine, argv, **kwargs), input=input, capture_output=True)


class DecidedEgressTest(unittest.TestCase):
    def test_bottle_host_is_routed_to_bottleds_socket_and_answered(self) -> None:
        answer = exec_(machine, ["wget", "-qO-", "-T", "5", "http://bottle.host/git/x"])
        self.assertEqual(answer, "answered by the service\n")
        [(address, port, name, said_machine, first)] = [f for f in stand_in.flows if b"/git/x" in f[4]]
        # Where the bottle connected, the name it asked for, and the machine: what bottle-cli says first.
        self.assertEqual((address, port, name, said_machine), (HOST_ADDRESS, 80, HOST, machine))
        self.assertTrue(first.startswith(b"GET /git/x "), first)

    def test_public_flows_go_direct(self) -> None:
        exec_(machine, ["wget", "-q", "-T", "10", "-O", "/dev/null", "https://example.com"])
        self.assertFalse([f for f in stand_in.flows if f[1] == 443])  # nothing of bottled's

    def test_a_denied_flow_fails_at_once(self) -> None:
        started = time.monotonic()
        result = run(["wget", "-q", "-T", "10", "-O", "/dev/null", f"http://{DENIED}/"])
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(time.monotonic() - started, 5)  # reset, not left to time out


class ExecTest(unittest.TestCase):
    def test_as_a_user_in_a_directory(self) -> None:
        self.assertEqual(exec_(machine, ["id", "-un"], user="root").strip(), "root")
        self.assertEqual(exec_(machine, ["sh", "-c", 'echo "$(id -un) $(pwd)"'], user="nobody", workdir="/tmp"),
                         "nobody /tmp\n")

    def test_the_machines_environment_reaches_every_user(self) -> None:
        for user in (None, "nobody"):
            with self.subTest(user=user):
                self.assertEqual(exec_(machine, ["sh", "-c", 'echo "$STANDIN"'], user=user), STANDIN + "\n")
        self.assertEqual(exec_(machine, ["sh", "-c", 'echo "$STANDIN"'], env={"STANDIN": "override"}),
                         "override\n")

    def test_the_commands_status_comes_back(self) -> None:
        for status in (0, 1, 7):
            with self.subTest(status=status):
                self.assertEqual(run(["sh", "-c", f"exit {status}"]).returncode, status)

    def test_bottle_clis_own_failures_say_so(self) -> None:
        self.assertIsNone(state(f"no-such-{machine}"))
        with self.assertRaisesRegex(Failed, f"^no machine no-such-{machine}"):
            exec_(f"no-such-{machine}", ["true"])


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
                argv = exec_command(machine, ["sh", "-c", "stty size; sleep 4; stty size"], tty=True)
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
                argv = exec_command(machine, ["sh", "-c", script], tty=True)
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
        self.assertEqual(exec_(machine, ["cat", "/mnt/ro/file"]), "from the host\n")
        self.assertNotEqual(run(["sh", "-c", "echo x > /mnt/ro/new"]).returncode, 0)
        self.assertFalse((work / "ro" / "new").exists())


class RestartTest(unittest.TestCase):
    def test_stop_and_start_keep_the_disk(self) -> None:
        exec_(machine, ["sh", "-c", "echo still here > /root/marker"])
        cli("stop", machine)
        self.assertNotEqual(state(machine), "running")
        cli("start", machine)
        self.assertEqual(state(machine), "running")
        self.assertEqual(exec_(machine, ["cat", "/root/marker"]), "still here\n")


class BuildTest(unittest.TestCase):
    """folio builds images with bottle-cli (builds.rs): a recipe's steps in a VM on a registry's image, its directory
    mounted where they run, its connections decided; then a machine boots the stage it made, with the environment
    the recipe set."""

    def test_a_built_image_boots(self) -> None:
        context = work / "probe"
        context.mkdir()
        (context / "install.sh").write_text("#!/bin/sh\nset -e\nwget -q -T 10 -O /dev/null https://example.com\n"
                                            "cat message > /hello\n")
        (context / "message").write_text("built by bottle-cli\n")
        recipe = {"pull": "alpine", "mount": {"source": str(context), "target": MOUNT},
                  "labels": {"bottle.inputs": "probe"},
                  "steps": [{"env": {"PROBE": "set by the recipe"}}, {"run": ["/bin/sh", "./install.sh"], "cwd": MOUNT}]}
        (work / "recipe.json").write_text(json.dumps(recipe))
        (work / "build-network.json").write_text(json.dumps({"cache": []}))
        disk = subprocess.run([BOTTLE_CLI, "image", "build", str(work / "recipe.json"), "--tag",
                               "localhost/bottle/probe:latest", "--network", str(work / "build-network.json")],
                              stdout=subprocess.PIPE, text=True, check=True).stdout.split()[-1]
        built = [i for i in json.loads(cli("images", "--json")) if i["tag"] == "localhost/bottle/probe:latest"]
        self.assertEqual(built[0]["labels"]["bottle.inputs"], "probe")
        name = f"{machine}-built"
        create(name, disk, [], {}, 1, network(stand_in.path))
        self.addCleanup(delete, name)
        cli("start", name)
        self.assertEqual(exec_(name, ["cat", "/hello"]), "built by bottle-cli\n")
        self.assertEqual(exec_(name, ["sh", "-c", 'echo "$PROBE"']), "set by the recipe\n")

if __name__ == "__main__":
    unittest.main()
