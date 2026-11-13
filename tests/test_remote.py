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

    def link(self) -> None:
        self.requests.append(("link", {}))

    def stream(self, op: str, request: str, data) -> str:
        self.requests.append((op, json.loads(request)))
        return remote.build(request, data)


class RemoteRunnerTest(unittest.TestCase):
    """A remote runner is the local runner where it runs, every operation's arguments and answer crossing as JSON."""

    def setUp(self) -> None:
        self.calls: list[tuple] = []
        self.machines: dict[str, str] = {}

        def record(name):
            return lambda *args, **kwargs: self.calls.append((name, args, kwargs))

        for name in ("create", "stop", "delete"):
            patcher = mock.patch.object(runners.krun, name, side_effect=record(name))
            patcher.start()
            self.addCleanup(patcher.stop)
        for target, value in (
            ((runners.krun, "state"), lambda name: self.machines.get(name)),
            ((runners.krun, "start"), record("start")),
            ((runners.krun, "exec_"), lambda name, argv, **kw: f"ran {argv} as {kw['user']} with {kw['input']!r}"),
            ((runners.daemon, "bind"), lambda *args: self.calls.append(("bind", args))),
            ((runners.daemon, "release_egress"), record("release")),
            ((runners.images, "archive_of"), lambda tag: Path("/images/base.tar") if tag == "base" else None),
        ):
            patcher = mock.patch.object(*target, side_effect=value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.transport = InProcess()
        self.runner = runners.RemoteRunner("box", self.transport)

    def test_a_machine_is_made_started_and_stopped_there(self) -> None:
        with mock.patch.object(runners.os, "cpu_count", return_value=32):   # the runner's cores, not the controller's
            self.runner.create("bottle-b", Path("/images/base.tar"), [runners.Mount(Path("/m/objects"), "/mnt/objects/w")],
                               {"A": "1"}, 512, 50)
        served = (Served(Path("/repo/.git"), (Remote("origin", "upstream"),), "workspace"),)
        self.runner.start("bottle-b", "b", served, ("tools",))
        self.runner.stop("bottle-b")
        self.runner.release("b")
        self.runner.delete("bottle-b")
        [create, bind, start, stop, release, delete] = self.calls
        self.assertEqual(create[1], ("bottle-b", "/images/base.tar",
                                     [runners.krun.Mount(Path("/m/objects"), "/mnt/objects/w")], {"A": "1"}, 512, 32, 50,
                                     None, None, None, None))
        self.assertEqual(bind[1], ("b", "bottle-b", served, ["tools"]))   # the repos it's served, as they were given
        self.assertEqual(start[1], ("bottle-b",))
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

    def test_an_attached_command_is_its_bottle_cli_exec_run_there(self) -> None:
        with mock.patch.object(runners.krun, "binary", return_value="/there/bottle-cli"):
            command = self.runner.exec_command("bottle-b", ["bash", "-l"], user="genie", workdir="/workspace", tty=True)
        self.assertEqual(command[:3], ["there", "-t", "/there/bottle-cli"])
        self.assertEqual(command[-3:], ["--", "bash", "-l"])

    def test_its_failures_are_the_controllers_errors_naming_it(self) -> None:
        with mock.patch.object(runners.krun, "stop", side_effect=BottleError("no such machine")), \
                self.assertRaisesRegex(BottleError, "runner box: no such machine"):
            self.runner.stop("bottle-b")

    def test_a_repo_is_mounted_from_its_mirror_there(self) -> None:
        repo = mock.Mock()
        with mock.patch.object(runners.repos, "objects_dir", return_value=Path("/src/app/.git/objects")), \
                mock.patch("bottle.mirrors.ensure", return_value=Path("/there/git/app-1234.git")) as ensure:
            self.assertEqual(self.runner.objects(repo), Path("/there/git/app-1234.git/objects"))
        from bottle import mirrors
        ensure.assert_called_once_with(mirrors.key(Path("/src/app/.git")))
        self.assertEqual(self.transport.requests[0], ("link", {}))


class RemoteBuildTest(unittest.TestCase):
    """A build on a runner: the controller works out what, the runner builds it from what it's sent."""

    def test_the_context_and_recipe_are_sent_and_built_there(self) -> None:
        import tempfile

        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(__import__("shutil").rmtree, tmp)
        (tmp / "context").mkdir()
        (tmp / "context" / "install.sh").write_text("echo hi")
        recipe = {"from": "localhost/bottle/base:latest", "steps": [{"run": ["true"]}],
                  "labels": {"bottle.features": "x"}}
        seen = {}

        def build(context, tag, inputs, recipe, no_cache):
            seen.update(files=sorted(p.name for p in context.iterdir()), args=(tag, inputs, recipe, no_cache))

        transport = InProcess()
        with mock.patch.object(runners.LocalRunner, "build", side_effect=lambda *a: build(*a)):
            runners.RemoteRunner("box", transport).build(tmp / "context", "localhost/bottle/base:with-x", "abc", recipe)
        self.assertEqual(seen, {"files": ["install.sh"],
                                "args": ("localhost/bottle/base:with-x", "abc", recipe, False)})
        self.assertEqual([op for op, _ in transport.requests], ["link", "build"])  # its build's flows need the link

    def test_a_build_that_fails_there_is_the_controllers_error(self) -> None:
        import tempfile

        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(__import__("shutil").rmtree, tmp)
        with mock.patch.object(runners.LocalRunner, "build", side_effect=BottleError("building x failed")), \
                self.assertRaisesRegex(BottleError, "runner box: building x failed"):
            runners.RemoteRunner("box", InProcess()).build(tmp, "t", "i", {"steps": []})


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
