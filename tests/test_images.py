import asyncio
import json
import subprocess
import unittest
from pathlib import Path
from unittest import mock

from bottle import images, runtime
from bottle.errors import BottleError


class RuntimeTest(unittest.TestCase):
    def test_network_gateway(self) -> None:
        inspect = [{"id": "default", "status": {"ipv4Gateway": "192.168.64.1", "ipv4Subnet": "192.168.64.0/24"}}]
        result = subprocess.CompletedProcess([], 0, stdout=json.dumps(inspect), stderr="")
        with mock.patch.object(runtime.subprocess, "run", return_value=result) as run:
            self.assertEqual(runtime.network_gateway(), "192.168.64.1")
        self.assertEqual(run.call_args.args[0], ["container", "network", "inspect", "default"])

    def test_failures_raise_with_stderr(self) -> None:
        result = subprocess.CompletedProcess([], 1, stdout="", stderr="network not found")
        with mock.patch.object(runtime.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(BottleError, "network not found"):
                runtime.network_gateway("nope")


class BuildCommandTest(unittest.IsolatedAsyncioTestCase):
    async def run_build(self, returncode: int = 0, **kwargs) -> list[str]:
        process = mock.AsyncMock()
        process.wait.return_value = returncode
        with mock.patch.object(runtime.asyncio, "create_subprocess_exec", return_value=process) as exec_:
            await runtime.build(Path("/ctx"), "bottle/base:latest", {"http_proxy": "http://gw:1"}, **kwargs)
        return list(exec_.call_args.args)

    async def test_command(self) -> None:
        self.assertEqual(
            await self.run_build(),
            ["container", "build", "--tag", "bottle/base:latest", "--progress", "auto",
             "--build-arg", "http_proxy=http://gw:1", "/ctx"],
        )

    async def test_never_quiet(self) -> None:
        cmd = await self.run_build(no_cache=True)
        self.assertIn("--no-cache", cmd)
        self.assertNotIn("-q", cmd)
        self.assertNotIn("--quiet", cmd)

    async def test_failure(self) -> None:
        with self.assertRaisesRegex(BottleError, "building bottle/base:latest failed"):
            await self.run_build(returncode=1)


class BuildImageTest(unittest.TestCase):
    def setUp(self) -> None:
        for name, value in {
            "ensure_container": mock.patch.object(images.prereqs, "ensure_container"),
            "builder_start": mock.patch.object(images.runtime, "builder_start"),
            "gateway": mock.patch.object(images.runtime, "network_gateway", return_value="127.0.0.1"),
        }.items():
            setattr(self, name, value.start())
            self.addCleanup(value.stop)

    def test_base_is_available(self) -> None:
        self.assertIn("base", images.available())

    def test_unknown_image(self) -> None:
        with self.assertRaisesRegex(BottleError, "no image named 'nope'; available: .*base"):
            images.build("nope")
        self.ensure_container.assert_not_called()

    def test_builds_through_a_live_proxy(self) -> None:
        seen = {}

        async def fake_build(context, tag, build_args, no_cache):
            seen.update(context=context, tag=tag, args=build_args, no_cache=no_cache)
            # The proxy must be serving while the build runs: ask it for something it refuses.
            host, port = build_args["https_proxy"].removeprefix("http://").split(":")
            reader, writer = await asyncio.open_connection(host, int(port))
            writer.write(b"CONNECT 127.0.0.1:22 HTTP/1.1\r\n\r\n")
            seen["proxy_answer"] = (await reader.readline()).strip()
            writer.close()

        with mock.patch.object(images.runtime, "build", side_effect=fake_build), self.assertLogs("bottle.egress"):
            self.assertEqual(images.build("base", no_cache=True), "bottle/base:latest")

        self.ensure_container.assert_called_once()
        self.builder_start.assert_called_once()
        self.assertEqual(seen["context"], images.CONTAINERS / "base")
        self.assertTrue(seen["no_cache"])
        self.assertEqual(set(seen["args"]), {"http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"})
        self.assertEqual(len(set(seen["args"].values())), 1)
        self.assertRegex(seen["args"]["http_proxy"], r"^http://127\.0\.0\.1:\d+$")
        self.assertEqual(seen["proxy_answer"], b"HTTP/1.1 403 Forbidden")

    def test_builder_starts_before_the_gateway_is_read(self) -> None:
        order = []
        self.builder_start.side_effect = lambda: order.append("builder")
        self.gateway.side_effect = lambda: order.append("gateway") or "127.0.0.1"
        with mock.patch.object(images.runtime, "build", mock.AsyncMock()):
            images.build("base")
        self.assertEqual(order, ["builder", "gateway"])


if __name__ == "__main__":
    unittest.main()
