import sys
import unittest
from unittest import mock

from bottle import runtime


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

    @unittest.skipUnless(sys.platform == "darwin", "host_resources reads a macOS sysctl (hw.memsize)")
    def test_the_real_host(self) -> None:
        cpus, memory = runtime.host_resources()
        self.assertGreaterEqual(cpus, 1)
        self.assertRegex(memory, r"^[1-9][0-9]*M$")


if __name__ == "__main__":
    unittest.main()
