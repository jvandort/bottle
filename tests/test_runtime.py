import sys
import unittest
from unittest import mock

from bottle import runtime
from bottle.errors import BottleError


class HostHasAddressTest(unittest.TestCase):
    def test_one_of_this_machines_addresses(self) -> None:
        self.assertTrue(runtime.host_has_address("127.0.0.1"))

    def test_an_address_no_interface_has(self) -> None:
        self.assertFalse(runtime.host_has_address("192.0.2.1"))  # TEST-NET-1


class InspectTest(unittest.TestCase):
    def inspect(self, returncode: int, stdout: str = "", services: bool = True):
        result = mock.Mock(returncode=returncode, stdout=stdout, stderr="")
        with mock.patch.object(runtime.subprocess, "run", return_value=result), \
                mock.patch.object(runtime, "services_running", return_value=services), \
                mock.patch.object(runtime, "start_services") as self.start_services:
            return runtime.container_state("c")

    def after_a_restart(self, inspect, found: str) -> tuple[object, list[list[str]]]:
        """Run `inspect` with the services down until something starts them; return its result and the commands run."""
        services = {"up": False}
        commands = []

        def run(cmd, **kwargs):
            commands.append(cmd)
            if cmd[:3] == ["container", "system", "start"]:
                services["up"] = True
                return mock.Mock(returncode=0)
            if cmd == ["container", "system", "status"]:
                return mock.Mock(returncode=0, stdout="status running\n" if services["up"] else "status stopped\n")
            return mock.Mock(returncode=0, stdout=found, stderr="") if services["up"] else \
                mock.Mock(returncode=1, stdout="", stderr='Error: internalError: "XPC connection error"')

        with mock.patch.object(runtime.subprocess, "run", side_effect=run), mock.patch("sys.stderr"):
            return inspect(), commands

    def test_found(self) -> None:
        self.assertEqual(self.inspect(0, '[{"status": {"state": "stopped"}}]'), "stopped")

    def test_absent(self) -> None:
        self.assertIsNone(self.inspect(1))

    def test_absent_when_inspect_finds_nothing(self) -> None:
        self.assertIsNone(self.inspect(0, "[]"))

    def test_absent_doesnt_start_running_services(self) -> None:
        self.inspect(1)
        self.start_services.assert_not_called()

    def test_after_a_restart_a_container_is_found_not_gone(self) -> None:
        state, commands = self.after_a_restart(lambda: runtime.container_state("c"), '[{"status": {"state": "stopped"}}]')
        self.assertEqual(state, "stopped")
        self.assertIn(["container", "system", "start", "--disable-kernel-install"], commands)

    def test_after_a_restart_an_image_is_found_not_missing(self) -> None:
        image = '[{"variants": [{"config": {"config": {"Labels": {"k": "v"}}}}]}]'
        labels, _ = self.after_a_restart(lambda: runtime.image_labels("bottle/base:latest"), image)
        self.assertEqual(labels, {"k": "v"})

    def test_after_a_restart_a_network_is_found_not_gone(self) -> None:
        exists, _ = self.after_a_restart(lambda: runtime.network_exists("n"), '[{}]')
        self.assertTrue(exists)

    def test_services_that_wont_start(self) -> None:
        failed = mock.Mock(returncode=1, stdout="", stderr="")
        status = mock.Mock(returncode=0, stdout="status stopped\n")
        with mock.patch.object(runtime.subprocess, "run",
                               side_effect=lambda cmd, **kw: status if cmd[1:3] == ["system", "status"] else failed), \
                mock.patch("sys.stderr"), self.assertRaisesRegex(BottleError, "`container system start` failed"):
            runtime.container_state("c")


class ContainerRunTest(unittest.TestCase):
    def test_a_bottle_gets_every_core_and_all_memory(self) -> None:
        with mock.patch.object(runtime, "_run") as run, mock.patch.object(runtime.os, "cpu_count", return_value=12), \
                mock.patch.object(runtime.subprocess, "run") as sysctl:
            sysctl.return_value.stdout = f"{64 * 1024 ** 3}\n"
            runtime.container_run("c", "img", "net", env={}, mounts=[])
        argv = run.call_args.args
        self.assertEqual(argv[argv.index("--cpus") + 1], "12")
        self.assertEqual(argv[argv.index("--memory") + 1], "65536M")
        self.assertEqual(argv[-1], "img")

    def test_a_bottle_can_get_less_memory(self) -> None:
        with mock.patch.object(runtime, "_run") as run, mock.patch.object(runtime.subprocess, "run") as sysctl:
            sysctl.return_value.stdout = f"{64 * 1024 ** 3}\n"
            runtime.container_run("c", "img", "net", env={}, mounts=[], memory="8G")
        argv = run.call_args.args
        self.assertEqual(argv[argv.index("--memory") + 1], "8G")

    @unittest.skipUnless(sys.platform == "darwin", "host_resources reads a macOS sysctl (hw.memsize)")
    def test_the_real_host(self) -> None:
        cpus, memory = runtime.host_resources()
        self.assertGreaterEqual(cpus, 1)
        self.assertRegex(memory, r"^[1-9][0-9]*M$")


class WithEnvTest(unittest.TestCase):
    def test_unsets_each_variable_before_setting_it(self) -> None:
        # So the command sees one copy, not the container's stale one first.
        self.assertEqual(
            runtime._with_env(["curl", "x"], {"HTTPS_PROXY": "http://a:1", "NO_PROXY": "localhost"}),
            ["env", "-u", "HTTPS_PROXY", "-u", "NO_PROXY", "HTTPS_PROXY=http://a:1", "NO_PROXY=localhost", "curl", "x"],
        )

    def test_no_env_leaves_the_command_alone(self) -> None:
        self.assertEqual(runtime._with_env(["bash", "-l"], None), ["bash", "-l"])


if __name__ == "__main__":
    unittest.main()
