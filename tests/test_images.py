import asyncio
import json
import subprocess
import tempfile
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


class DependencyTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.containers = Path(tmp.name)
        patcher = mock.patch.object(images, "IMAGES", self.containers)
        patcher.start()
        self.addCleanup(patcher.stop)

    def image(self, name: str, dockerfile: str) -> None:
        (self.containers / name).mkdir()
        (self.containers / name / "Dockerfile").write_text(dockerfile)

    def test_real_images(self) -> None:
        self.assertEqual(images.available.__module__, "bottle.images")
        with mock.patch.object(images, "IMAGES", Path(images.__file__).resolve().parent.parent / "containers" / "images"):
            self.assertIn("base", images.available())
            self.assertEqual(images.dependencies("base"), [])

    def test_from_lines(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("a", "FROM --platform=linux/arm64 bottle/base:latest AS builder\nFROM bottle/base\n")
        self.assertEqual(images.dependencies("a"), ["base"])
        self.assertEqual(images.dependencies("base"), [])

    def test_order_is_dependencies_first(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("tools", "FROM bottle/base:latest\n")
        self.image("agent", "FROM bottle/tools:latest\n")
        self.image("multi", "FROM bottle/agent:latest AS a\nFROM bottle/tools:latest\n")
        self.assertEqual(images.build_order("multi"), ["base", "tools", "agent", "multi"])

    def test_cycle(self) -> None:
        self.image("a", "FROM bottle/b\n")
        self.image("b", "FROM bottle/a\n")
        with self.assertRaisesRegex(BottleError, "cycle: a -> b -> a"):
            images.build_order("a")

    def test_missing_dependency(self) -> None:
        self.image("a", "FROM bottle/ghost\n")
        with self.assertRaisesRegex(BottleError, "a is built from bottle/ghost, but there's no containers/images/ghost"):
            images.build_order("a")

    def test_ensure_built_builds_whats_missing_or_stale(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("tools", "FROM bottle/base:latest\n")
        built = []
        for exists, expected in ((set(), ["base", "tools"]), ({"bottle/base:latest"}, ["tools"]),
                                 ({"bottle/base:latest", "bottle/tools:latest"}, [])):
            built.clear()
            with self.subTest(exists=exists), \
                    mock.patch.object(images, "is_current", side_effect=lambda t, inputs: t in exists), \
                    mock.patch.object(images, "_build_one", side_effect=lambda n, no_cache=False: built.append(n)), \
                    mock.patch("sys.stderr"):
                images.ensure_built("tools")
                self.assertEqual(built, expected)

    def test_build_rebuilds_the_image_but_not_built_dependencies(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("tools", "FROM bottle/base:latest\n")
        built = []
        with mock.patch.object(images, "is_current", return_value=True), \
                mock.patch.object(images, "_build_one", side_effect=lambda n, no_cache=False: built.append((n, no_cache))):
            self.assertEqual(images.build("tools", no_cache=True), "bottle/tools:latest")
        self.assertEqual(built, [("tools", True)])


class StalenessTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        (self.dir / "Dockerfile").write_text("FROM debian:13\n")

    def test_tree_hash_tracks_names_contents_and_the_executable_bit(self) -> None:
        before = images.tree_hash(self.dir)
        self.assertEqual(images.tree_hash(self.dir), before)
        (self.dir / "Dockerfile").write_text("FROM debian:14\n")
        after_edit = images.tree_hash(self.dir)
        (self.dir / "Dockerfile").chmod(0o755)
        after_chmod = images.tree_hash(self.dir)
        (self.dir / "extra").write_text("")
        self.assertEqual(len({before, after_edit, after_chmod, images.tree_hash(self.dir)}), 4)

    def test_is_current_compares_the_label(self) -> None:
        with mock.patch.object(images.runtime, "image_labels", return_value={"bottle.inputs": "abc"}):
            self.assertTrue(images.is_current("bottle/base:latest", "abc"))
            self.assertFalse(images.is_current("bottle/base:latest", "def"))
        with mock.patch.object(images.runtime, "image_labels", return_value=None):
            self.assertFalse(images.is_current("bottle/base:latest", "abc"))  # missing
        with mock.patch.object(images.runtime, "image_labels", return_value={}):
            self.assertFalse(images.is_current("bottle/base:latest", "abc"))  # built before labels


class BuildImageTest(unittest.TestCase):
    def setUp(self) -> None:
        for name, value in {
            "ensure_container": mock.patch.object(images.prereqs, "ensure_container"),
            "builder_start": mock.patch.object(images.runtime, "builder_start"),
            "builder_running": mock.patch.object(images.runtime, "builder_running", return_value=False),
            "builder_stop": mock.patch.object(images.runtime, "builder_stop"),
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

        async def fake_build(context, tag, build_args, no_cache, dockerfile, labels):
            seen.update(context=context, tag=tag, args=build_args, no_cache=no_cache, labels=labels)
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
        self.assertEqual(seen["context"], images.IMAGES / "base")
        self.assertTrue(seen["no_cache"])
        self.assertEqual(set(seen["args"]), {"http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"})
        self.assertEqual(len(set(seen["args"].values())), 1)
        self.assertRegex(seen["args"]["http_proxy"], r"^http://127\.0\.0\.1:\d+$")
        self.assertEqual(seen["proxy_answer"], b"HTTP/1.1 403 Forbidden")
        self.assertEqual(seen["labels"], {"bottle.inputs": images.inputs_hash("base")})

    def test_builder_starts_before_the_gateway_is_read(self) -> None:
        order = []
        self.builder_start.side_effect = lambda: order.append("builder")
        self.gateway.side_effect = lambda: order.append("gateway") or "127.0.0.1"
        with mock.patch.object(images.runtime, "build", mock.AsyncMock()):
            images.build("base")
        self.assertEqual(order, ["builder", "gateway"])


    def test_stops_the_builder_it_started(self) -> None:
        with mock.patch.object(images.runtime, "build", mock.AsyncMock()):
            images.build("base")
        self.builder_stop.assert_called_once()

    def test_leaves_a_builder_that_was_already_running(self) -> None:
        self.builder_running.return_value = True
        with mock.patch.object(images.runtime, "build", mock.AsyncMock()):
            images.build("base")
        self.builder_stop.assert_not_called()

    def test_builds_inside_one_session_share_the_builder(self) -> None:
        order = []
        self.builder_stop.side_effect = lambda: order.append("stop")
        with mock.patch.object(images.runtime, "build", mock.AsyncMock(side_effect=lambda *a: order.append("build"))):
            with images.building():
                images.build("base")
                self.builder_running.return_value = True  # it's running now: bottle started it
                images.build("base")
        self.assertEqual(order, ["build", "build", "stop"])

    def test_a_failed_build_still_stops_the_builder(self) -> None:
        with mock.patch.object(images.runtime, "build", mock.AsyncMock(side_effect=BottleError("boom"))), \
                self.assertRaises(BottleError):
            images.build("base")
        self.builder_stop.assert_called_once()

    def test_failing_to_stop_the_builder_doesnt_fail_the_build(self) -> None:
        self.builder_stop.side_effect = BottleError("nope")
        with mock.patch.object(images.runtime, "build", mock.AsyncMock()), mock.patch("sys.stderr"):
            self.assertEqual(images.build("base"), "bottle/base:latest")


if __name__ == "__main__":
    unittest.main()
