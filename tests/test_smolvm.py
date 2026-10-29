import json
import subprocess
import unittest
from pathlib import Path
from unittest import mock

from bottle import bottles, host, smolvm
from bottle.errors import BottleError
from tests.support import GitTestCase


def completed(stdout: str = "", stderr: str = "", returncode: int = 0) -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess([], returncode, stdout, stderr)


class CommandTest(unittest.TestCase):
    """What bottle asks of smolvm, word for word."""

    def run_with(self, result: subprocess.CompletedProcess | None = None):
        patcher = mock.patch.object(smolvm.subprocess, "run", return_value=result or completed())
        self.addCleanup(patcher.stop)
        return patcher.start()

    def test_create_makes_a_mediated_machine_with_read_only_mounts(self) -> None:
        run = self.run_with()
        smolvm.create("bottle-b", "/images/b.tar", [smolvm.Mount(Path("/repo/.git/objects"), "/mnt/repo/objects")],
                      {"TOKEN": "stand-in"}, memory_mib=8192, cpus=1000)
        self.assertEqual(run.call_args.args[0], [
            "smolvm", "--error-format", "json", "machine", "create", "--name", "bottle-b", "--image", "/images/b.tar",
            "--net", "--net-backend", "virtio-net", "--cpus", "255", "--mem", "8192",
            "-v", "/repo/.git/objects:/mnt/repo/objects:ro", "-e", "TOKEN=stand-in",
        ])

    def test_start_binds_the_decider_with_the_token_in_the_environment_only(self) -> None:
        run = self.run_with()
        smolvm.start("bottle-b", "127.0.0.1:47329", "ab" * 32)
        argv, env = run.call_args.args[0], run.call_args.kwargs["env"]
        self.assertEqual(argv[3:], ["machine", "start", "--name", "bottle-b", "--egress-interceptor", "127.0.0.1:47329",
                                    "--mediated-egress"])
        self.assertEqual(env["SMOLVM_INTERCEPTOR_TOKEN"], "ab" * 32)
        self.assertNotIn("ab" * 32, " ".join(argv))

    def test_start_raises_the_tcp_relay_capacity(self) -> None:
        run = self.run_with()
        smolvm.start("bottle-b", "127.0.0.1:47329", "ab" * 32)
        self.assertEqual(run.call_args.kwargs["env"]["SMOLVM_TCP_RELAY_CAPACITY"], "4096")

    def test_exec_is_always_interactive(self) -> None:
        # A plain exec cuts output at 11 MiB without saying so.
        self.assertEqual(smolvm.exec_args("m", ["cat", "f"]), ["machine", "exec", "--name", "m", "-i", "--", "cat", "f"])
        self.assertEqual(
            smolvm.exec_args("m", ["sh"], user="genie", workdir="/workspace", env={"A": "1"}, tty=True),
            ["machine", "exec", "--name", "m", "-i", "-t", "-u", "genie", "-w", "/workspace", "-e", "A=1", "--", "sh"],
        )

    def test_exec_gives_the_command_stdin_and_returns_its_output(self) -> None:
        run = self.run_with(completed("out"))
        self.assertEqual(smolvm.exec_("m", ["cat"], input="in"), "out")
        self.assertEqual(run.call_args.kwargs["input"], "in")

    def test_the_command_line_for_a_terminal(self) -> None:
        self.assertEqual(smolvm.exec_command("m", ["claude"], user="genie", tty=True)[:3],
                         ["smolvm", "--error-format", "json"])

    def test_state(self) -> None:
        self.run_with(completed(json.dumps({"state": "running", "name": "m"})))
        self.assertEqual(smolvm.state("m"), "running")

    def test_no_such_machine_has_no_state(self) -> None:
        for error in ('{"error":{"code":"NOT_FOUND","message":"vm not found: m"}}',
                      # As `machine status` says it.
                      '{"error":{"code":"CONFIG_ERROR","message":"config operation failed: machine status: '
                      'machine \'m\' not found"}}'):
            with self.subTest(error):
                self.run_with(completed(stderr=error, returncode=1))
                self.assertIsNone(smolvm.state("m"))

    def test_smolvms_own_failures_are_its_message(self) -> None:
        self.run_with(completed(stderr='{"error":{"code":"MOUNT_ERROR","message":"no such directory"}}', returncode=1))
        with self.assertRaisesRegex(BottleError, "^no such directory$"):
            smolvm.stop("m")

    def test_a_commands_failure_is_its_stderr(self) -> None:
        self.run_with(completed(stderr="sh: nope: not found\n", returncode=127))
        with self.assertRaisesRegex(BottleError, "nope: not found"):
            smolvm.exec_("m", ["nope"])

    def test_deleting_a_machine_that_isnt_there(self) -> None:
        run = self.run_with(completed(stderr='{"error":{"code":"NOT_FOUND","message":"gone"}}', returncode=1))
        smolvm.delete("m")
        self.assertEqual(run.call_count, 1)  # the status, and no delete


class SmolvmBottleTest(GitTestCase):
    """A bottle: its machine made, started, stopped and deleted, with smolvm and bottled stood in for."""

    def setUp(self) -> None:
        super().setUp()
        self.repo_path = self.make_repo("example")
        self.calls: list[str] = []
        self.machines: dict[str, str] = {}
        self.created: dict = {}
        archive = self.tmp / "base.tar"
        archive.write_bytes(b"archive")

        def exec_(name, argv, user=None, workdir=None, input=None, env=None):
            script = argv[-1]
            what = ("contract" if "# bottle contract" in script else "trust CA" if bottles.CA_CERTIFICATE in script
                    else "bottle.host" if host.HOST in script and "/etc/hosts" in script else "workspace")
            self.calls.append(f"{what} as {user}")
            return ""

        def create(name, image, mounts, env, memory_mib, cpus):
            self.calls.append("create")
            self.machines[name] = "created"
            self.created = dict(name=name, image=image, mounts=mounts, env=env, memory_mib=memory_mib)

        def start(name, decider, token):
            self.calls.append(f"start {decider} {token}")
            self.machines[name] = "running"

        def stop(name):
            self.calls.append("stop")
            self.machines[name] = "stopped"

        def delete(name):
            self.calls.append("delete")
            self.machines.pop(name, None)

        bind = lambda bottle, *args: (self.calls.append(f"bind {bottle}"), ("127.0.0.1:47329", "token"))[1]
        for target, name, value in (
            (bottles.smolvm, "create", create), (bottles.smolvm, "start", start), (bottles.smolvm, "stop", stop),
            (bottles.smolvm, "delete", delete), (bottles.smolvm, "exec_", exec_),
            (bottles.smolvm, "state", lambda name: self.machines.get(name)),
            (bottles.images, "archive_of", lambda tag: archive),
            (bottles.daemon, "bind_decider", bind),
            (bottles.daemon, "release_egress", lambda bottle: self.calls.append(f"release {bottle}")),
            (bottles.prereqs, "ensure_smolvm", lambda: None),
            (bottles.features_, "ensure_built", lambda image, specs: "localhost/bottle/base:latest"),
            (bottles.features_, "remove_stale", lambda: []),
            (bottles.auth, "ensure_logged_in", lambda specs: None),
            (bottles.auth, "standins_for", lambda specs: {"CLAUDE_CODE_OAUTH_TOKEN": "stand-in"}),
            (bottles.auth, "intercepted_hosts", lambda specs: []),
        ):
            patcher = mock.patch.object(target, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def create(self, memory: str | None = "8G", mac_mib: int = 16384) -> bottles.Bottle:
        spec = {"features": [], **({"memory": memory} if memory else {})}
        with mock.patch.object(bottles.specs, "host_memory_mib", return_value=mac_mib):
            return bottles.create(spec, "b", [bottles.MountRequest("/workspace", self.repo_path)])

    def test_a_new_bottle_is_a_machine_bound_to_the_decider(self) -> None:
        bottle = self.create()
        self.assertEqual(bottle.archive, "base.tar")
        self.assertEqual(bottles.get("b").archive, "base.tar")
        self.assertEqual(self.calls, [
            "create", "bind b", "start 127.0.0.1:47329 token", "contract as genie", "trust CA as root",
            "bottle.host as root", "workspace as root", "workspace as genie",
        ])

    def test_its_memory_must_be_set(self) -> None:
        for memory in (None, "all"):
            with self.subTest(memory), self.assertRaisesRegex(BottleError, "memory"):
                self.create(memory=memory)
        self.assertEqual(self.machines, {})

    def test_its_memory_is_the_specs(self) -> None:
        self.create(memory="512M")
        self.assertEqual(self.created["memory_mib"], 512)

    def test_its_machine_has_the_repos_objects_read_only_stand_ins_and_no_proxy(self) -> None:
        self.create(memory="8G")
        self.assertEqual([m.target for m in self.created["mounts"]], ["/mnt/objects/workspace"])
        self.assertEqual(self.created["env"]["CLAUDE_CODE_OAUTH_TOKEN"], "stand-in")
        self.assertFalse({"http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"} & set(self.created["env"]))
        self.assertEqual(self.created["memory_mib"], 8192)

    def test_a_stopped_bottle_starts_with_a_new_binding_and_its_setup_again(self) -> None:
        bottle = self.create()
        bottles.stop("b")
        self.assertEqual(self.calls[-2:], ["stop", "release b"])
        self.calls.clear()
        bottles.ensure_running("b")
        self.assertEqual(self.calls, ["bind b", "start 127.0.0.1:47329 token", "contract as genie",
                                      "trust CA as root", "bottle.host as root"])
        self.assertEqual(self.machines[bottle.machine], "running")

    def test_a_running_bottle_keeps_its_binding_and_has_bottled_running(self) -> None:
        self.create()
        self.calls.clear()
        with mock.patch.object(bottles.daemon, "start", side_effect=lambda: self.calls.append("bottled")):
            bottles.ensure_running("b")
        self.assertEqual(self.calls, ["bottled"])

    def test_its_state_is_the_machines(self) -> None:
        bottle = self.create()
        self.assertEqual(bottles.status(["b"])[0]["state"], "running")
        self.machines.pop(bottle.machine)
        self.assertEqual(bottles.status(["b"])[0]["state"], "missing")

    def test_delete_takes_the_machine_and_its_binding(self) -> None:
        self.create()
        self.calls.clear()
        bottles.delete("b")
        self.assertEqual(self.calls, ["release b", "delete"])
        self.assertNotIn("b", bottles.load())

    def test_a_failed_start_rolls_the_bottle_back(self) -> None:
        with mock.patch.object(bottles.smolvm, "start", side_effect=BottleError("no hypervisor")):
            with self.assertRaisesRegex(BottleError, "no hypervisor"):
                self.create()
        self.assertEqual(self.machines, {})
        self.assertNotIn("b", bottles.load())

    def test_shutdown_stops_its_machines(self) -> None:
        self.create()
        with mock.patch.object(bottles.daemon, "stop", return_value=True):
            self.assertEqual(bottles.shutdown(), (["b"], True))



if __name__ == "__main__":
    unittest.main()
