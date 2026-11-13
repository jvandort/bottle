import json
import subprocess
import unittest
from pathlib import Path
from unittest import mock

from bottle import bottles, daemon, host, krun
from bottle.errors import BottleError
from tests.support import GitTestCase, stop_bottle, thread_dir


def completed(stdout: str = "", stderr: str = "", returncode: int = 0) -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess([], returncode, stdout, stderr)


class CommandTest(unittest.TestCase):
    """What bottle asks of bottle-cli, word for word."""

    def run_with(self, result: subprocess.CompletedProcess | None = None):
        patcher = mock.patch.object(krun.subprocess, "run", return_value=result or completed())
        self.addCleanup(patcher.stop)
        return patcher.start()

    def test_create_makes_a_machine_with_read_only_mounts(self) -> None:
        run = self.run_with()
        krun.create("bottle-b", "/stages/b.ext4", [krun.Mount(Path("/repo/.git/objects"), "/mnt/repo/objects")],
                    {"TOKEN": "stand-in"}, memory_mib=8192, cpus=32, disk_gib=50)
        self.assertEqual(run.call_args.args[0], [
            krun.binary(), "create", "bottle-b", "--image", "/stages/b.ext4",
            "--cpus", "32", "--mem", "8192", "--storage", "50",
            "-v", "/repo/.git/objects:/mnt/repo/objects:ro", "-e", "TOKEN=stand-in",
        ])

    def test_create_with_bottle_clis_defaults(self) -> None:
        run = self.run_with()
        krun.create("m", "/stages/b.ext4", [], {}, memory_mib=None, cpus=2)
        self.assertEqual(run.call_args.args[0][1:], ["create", "m", "--image", "/stages/b.ext4", "--cpus", "2"])

    def test_create_gives_its_users_defaults_and_labels(self) -> None:
        run = self.run_with()
        krun.create("fix-it", "/stages/b.ext4", [], {}, memory_mib=None, cpus=2, user="genie", workdir="/workspace",
                    labels={"thread": "fix-it"})
        self.assertEqual(run.call_args.args[0][-6:], ["--user", "genie", "--workdir", "/workspace",
                                                      "--label", "thread=fix-it"])

    def test_start_needs_only_the_machine(self) -> None:
        run = self.run_with()
        krun.start("b")
        self.assertEqual(run.call_args.args[0][1:], ["start", "b"])

    def test_a_network_policy_is_given_on_stdin_at_create_and_configure(self) -> None:
        given = []
        run = self.run_with()
        run.side_effect = lambda argv, **kw: (given.append((argv[argv.index("--network") + 1], json.loads(kw["input"]))),
                                              subprocess.CompletedProcess(argv, 0, "", ""))[1]
        policy = {"routes": [{"address": "192.0.2.1", "port": 80, "socket": "/s"}]}
        krun.create("b", "/stages/b.ext4", [], {}, memory_mib=None, cpus=2, network=policy)
        krun.configure("b", policy)
        self.assertEqual(given, [("-", policy), ("-", policy)])
        self.assertEqual(run.call_args.args[0][1:4], ["configure", "b", "--network"])

    def test_exec_args(self) -> None:
        self.assertEqual(krun.exec_args("m", ["cat", "f"]), ["exec", "m", "--", "cat", "f"])
        self.assertEqual(
            krun.exec_args("m", ["sh"], user="genie", workdir="/workspace", env={"A": "1"}, tty=True),
            ["exec", "m", "-t", "-u", "genie", "-w", "/workspace", "-e", "A=1", "--", "sh"],
        )

    def test_exec_gives_the_command_stdin_and_returns_its_output(self) -> None:
        run = self.run_with(completed("out"))
        self.assertEqual(krun.exec_("m", ["cat"], input="in"), "out")
        self.assertEqual(run.call_args.kwargs["input"], "in")

    def test_the_command_line_for_a_terminal(self) -> None:
        self.assertEqual(krun.exec_command("m", ["claude"], user="genie", tty=True)[:3],
                         [krun.binary(), "exec", "m"])

    def test_the_binary_is_the_repos_dist_unless_named(self) -> None:
        with mock.patch.dict(krun.os.environ, {"BOTTLE_CLI": ""}):
            self.assertEqual(krun.binary(), str(krun.REPO / "target" / "dist" / "bottle-cli"))
        with mock.patch.dict(krun.os.environ, {"BOTTLE_CLI": "/opt/bottle-cli"}):
            self.assertEqual(krun.binary(), "/opt/bottle-cli")

    def test_state(self) -> None:
        self.run_with(completed(json.dumps({"state": "running", "name": "m"})))
        self.assertEqual(krun.state("m"), "running")

    def test_running_is_every_machine_not_stopped(self) -> None:
        self.run_with(completed(json.dumps([{"name": "a", "state": "running"}, {"name": "b", "state": "stopped"},
                                            {"name": "c", "state": "created"}])))
        self.assertEqual(krun.running(), ["a"])

    def test_no_such_machine_has_no_state(self) -> None:
        self.run_with(completed(stderr="bottle-cli: no machine m\n", returncode=1))
        self.assertIsNone(krun.state("m"))

    def test_bottle_clis_own_failures_are_its_message(self) -> None:
        self.run_with(completed(stderr="bottle-cli: stopping m: the agent went away; killed\n", returncode=1))
        with self.assertRaisesRegex(BottleError, "^stopping m: the agent went away; killed$"):
            krun.stop("m")

    def test_a_commands_failure_is_its_stderr(self) -> None:
        self.run_with(completed(stderr="sh: nope: not found\n", returncode=127))
        with self.assertRaisesRegex(BottleError, "nope: not found"):
            krun.exec_("m", ["nope"])

    def test_deleting_a_machine_that_isnt_there(self) -> None:
        run = self.run_with(completed(stderr="bottle-cli: no machine m\n", returncode=1))
        krun.delete("m")
        self.assertEqual(run.call_count, 1)  # the status, and no delete


class KrunBottleTest(GitTestCase):
    """A bottle: its machine made, started, stopped and deleted, with bottle-cli and bottled stood in for."""

    def setUp(self) -> None:
        super().setUp()
        self.repo_path = self.make_repo("example")
        self.calls: list[str] = []
        self.machines: dict[str, str] = {}
        self.created: dict = {}

        def exec_(name, argv, user=None, workdir=None, input=None, env=None, cli=None):
            script = argv[-1]
            what = ("contract" if "# bottle contract" in script else "trust CA" if bottles.CACHE_CERTIFICATE in script
                    else "bottle.host" if host.HOST in script and "/etc/hosts" in script else "workspace")
            self.calls.append(f"{what} as {user}")
            return ""

        def create(name, image, mounts, env, memory_mib, cpus, disk_gib=None, user=None, workdir=None, labels=None,
                   network=None, cli=None):
            self.calls.append("create")
            self.machines[name] = "created"
            self.created = dict(name=name, image=image, mounts=mounts, env=env, memory_mib=memory_mib)

        def start(name, cli=None):
            self.calls.append("start")
            self.machines[name] = "running"

        def stop(name, cli=None):
            self.calls.append("stop")
            self.machines[name] = "stopped"

        def delete(name, cli=None):
            self.calls.append("delete")
            self.machines.pop(name, None)

        bind = lambda bottle, *args: self.calls.append(f"bind {bottle}")
        for target, name, value in (
            (bottles.runners.krun, "create", create), (bottles.runners.krun, "start", start), (bottles.runners.krun, "stop", stop),
            (bottles.runners.krun, "delete", delete), (bottles.runners.krun, "exec_", exec_),
            (bottles.runners.krun, "state", lambda name, cli=None: self.machines.get(name)),
            (bottles.images, "built_labels", lambda tag, cli=None: {}),
            (daemon, "bind", bind),
            (daemon, "release_egress", lambda bottle: self.calls.append(f"release {bottle}")),
            (bottles.krun.prereqs, "ensure_cli", lambda: None),
            (bottles.features_, "ensure_built", lambda specs, runner=None: "localhost/bottle/base:latest"),
            (bottles.features_, "remove_stale", lambda runner=None: []),
            (bottles.auth, "warn_unset", lambda specs: None),
            (bottles.auth, "standins_for", lambda specs: {"CLAUDE_CODE_OAUTH_TOKEN": "stand-in"}),
            (bottles.auth, "credential_destinations", lambda specs: []),
            (bottles.runners.krun, "ca_certificate", lambda name, hosts, cli=None: None),
        ):
            patcher = mock.patch.object(target, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def create(self, memory: str | None = "8G", mac_mib: int = 16384) -> bottles.Bottle:
        spec = {"features": [], **({"memory": memory} if memory else {})}
        with mock.patch.object(bottles.specs, "host_memory_mib", return_value=mac_mib):
            return bottles.create(thread_dir(), spec, "b", [bottles.MountRequest("/workspace", self.repo_path)])

    def test_a_new_bottle_is_a_machine_bound_to_the_decider(self) -> None:
        bottle = self.create()
        self.assertEqual(self.calls, [
            "create", "bind b", "start", "contract as genie", "trust CA as root",
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

    def test_a_stopped_bottle_starts_bound_again(self) -> None:
        bottle = self.create()
        stop_bottle("b")
        self.assertEqual(self.calls[-2:], ["stop", "release b"])
        self.calls.clear()
        bottles.ensure_running("b")
        self.assertEqual(self.calls, ["bind b", "start"])
        self.assertEqual(self.machines[bottle.machine], "running")

    def test_a_running_bottle_keeps_its_binding_and_has_bottled_running(self) -> None:
        self.create()
        self.calls.clear()
        with mock.patch.object(daemon, "start", side_effect=lambda: self.calls.append("bottled")):
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
        with mock.patch.object(bottles.runners.krun, "start", side_effect=BottleError("no hypervisor")):
            with self.assertRaisesRegex(BottleError, "no hypervisor"):
                self.create()
        self.assertEqual(self.machines, {})
        self.assertNotIn("b", bottles.load())


if __name__ == "__main__":
    unittest.main()
