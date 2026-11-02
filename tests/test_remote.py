import json
import unittest
from pathlib import Path
from unittest import mock

from bottle import remote, runners
from bottle.errors import BottleError
from bottle.host import Remote, Served


class InProcess:
    """A transport to a runner that is this process: what ssh will carry, without ssh."""

    def __init__(self) -> None:
        self.requests: list[tuple[str, dict]] = []

    def call(self, op: str, request: str) -> str:
        self.requests.append((op, json.loads(request)))
        return remote.answer(op, request)

    def command(self, argv: list[str], tty: bool) -> list[str]:
        return ["there", *(["-t"] if tty else []), *argv]


class RemoteRunnerTest(unittest.TestCase):
    """A remote runner is the local runner where it runs, every operation's arguments and answer crossing as JSON."""

    def setUp(self) -> None:
        self.calls: list[tuple] = []
        self.machines: dict[str, str] = {}

        def record(name):
            return lambda *args, **kwargs: self.calls.append((name, args, kwargs))

        for name in ("create", "stop", "delete"):
            patcher = mock.patch.object(runners.smolvm, name, side_effect=record(name))
            patcher.start()
            self.addCleanup(patcher.stop)
        for target, value in (
            ((runners.smolvm, "state"), lambda name: self.machines.get(name)),
            ((runners.smolvm, "start"), record("start")),
            ((runners.smolvm, "exec_"), lambda name, argv, **kw: f"ran {argv} as {kw['user']} with {kw['input']!r}"),
            ((runners.daemon, "bind_decider"), lambda *args: self.calls.append(("bind", args)) or ("127.0.0.1:1", "t")),
            ((runners.daemon, "release_egress"), record("release")),
            ((runners.images, "archive_of"), lambda tag: Path("/images/base.tar") if tag == "base" else None),
        ):
            patcher = mock.patch.object(*target, side_effect=value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.transport = InProcess()
        self.runner = runners.RemoteRunner("box", self.transport)

    def test_a_machine_is_made_started_and_stopped_there(self) -> None:
        self.runner.create("bottle-b", Path("/images/base.tar"), [runners.Mount(Path("/m/objects"), "/mnt/objects/w")],
                           {"A": "1"}, 512, 4)
        served = (Served(Path("/repo/.git"), (Remote("origin", "upstream"),), "workspace"),)
        self.runner.start("bottle-b", "b", served, ("tools",))
        self.runner.stop("bottle-b")
        self.runner.release("b")
        self.runner.delete("bottle-b")
        [create, bind, start, stop, release, delete] = self.calls
        self.assertEqual(create[1], ("bottle-b", "/images/base.tar",
                                     [runners.smolvm.Mount(Path("/m/objects"), "/mnt/objects/w")], {"A": "1"}, 512, 4))
        self.assertEqual(bind[1], ("b", served, ["tools"]))   # the repos it's served, as they were given
        self.assertEqual(start[1], ("bottle-b", "127.0.0.1:1", "t"))
        self.assertEqual([c[0] for c in (stop, release, delete)], ["stop", "release", "delete"])

    def test_its_state_and_images_are_its_own(self) -> None:
        self.machines["bottle-b"] = "running"
        self.assertEqual(self.runner.state("bottle-b"), "running")
        self.assertIsNone(self.runner.state("bottle-c"))
        self.assertEqual(self.runner.archive("base"), Path("/images/base.tar"))
        self.assertIsNone(self.runner.archive("other"))

    def test_a_command_runs_there_with_its_input(self) -> None:
        self.assertEqual(self.runner.exec_("bottle-b", ["cat"], user="root", input="pem"),
                         "ran ['cat'] as root with 'pem'")

    def test_an_attached_command_is_its_smolvm_exec_run_there(self) -> None:
        with mock.patch.object(runners.smolvm, "binary", return_value="/there/smolvm"):
            command = self.runner.exec_command("bottle-b", ["bash", "-l"], user="genie", workdir="/workspace", tty=True)
        self.assertEqual(command[:3], ["there", "-t", "/there/smolvm"])
        self.assertEqual(command[-3:], ["--", "bash", "-l"])

    def test_its_failures_are_the_controllers_errors_naming_it(self) -> None:
        with mock.patch.object(runners.smolvm, "stop", side_effect=BottleError("no such machine")), \
                self.assertRaisesRegex(BottleError, "runner box: no such machine"):
            self.runner.stop("bottle-b")

    def test_no_repo_can_be_mounted_until_it_has_mirrors(self) -> None:
        with self.assertRaisesRegex(BottleError, "no mirrors"):
            self.runner.objects(mock.Mock())


class AnswerTest(unittest.TestCase):
    def test_an_operation_there_isnt(self) -> None:
        self.assertEqual(json.loads(remote.answer("format", "{}")), {"ok": False, "error": "no runner operation 'format'"})

    def test_run_as_a_command_json_in_and_out(self) -> None:
        import subprocess
        import sys

        root = Path(__file__).resolve().parents[1]
        result = subprocess.run([sys.executable, "-m", "bottle.remote", "nope"], input="{}", capture_output=True,
                                text=True, cwd=root)
        self.assertEqual((result.returncode, json.loads(result.stdout)),
                         (0, {"ok": False, "error": "no runner operation 'nope'"}))
