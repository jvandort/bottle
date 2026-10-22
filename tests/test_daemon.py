import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import daemon
from bottle.daemon import Daemon
from bottle.decider import Decider
from bottle.errors import BottleError


class SocketPathTest(unittest.TestCase):
    def test_lives_in_bottle_home(self) -> None:
        with mock.patch.dict(os.environ, {"BOTTLE_HOME": "/Users/me/.bottle"}):
            self.assertEqual(daemon.socket_path(), Path("/Users/me/.bottle/bottled.sock"))

    def test_deep_bottle_home_gets_a_short_path(self) -> None:
        deep = "/Users/me/" + "x" * 120
        with mock.patch.dict(os.environ, {"BOTTLE_HOME": deep}):
            path = daemon.socket_path()
        self.assertLessEqual(len(str(path)), daemon.MAX_SOCKET_PATH)
        self.assertTrue(str(path).startswith(f"/tmp/bottle-{os.getuid()}/"))


class DaemonTest(unittest.IsolatedAsyncioTestCase):
    """bottled itself; what the decider does with a bottle's flows is test_decider's."""

    async def asyncSetUp(self) -> None:
        # Keep the socket path short: macOS caps Unix socket paths at 104 bytes.
        tmp = tempfile.mkdtemp(dir="/tmp")
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        self.tmp = Path(tmp)
        self.path = self.tmp / "d.sock"
        self.daemon = Daemon(decider=Decider(self.tmp / "tokens.json"), decider_port=0)
        for patcher in (
            # The features these tests name aren't resolved: what a credential claims is mocked where it matters.
            mock.patch.object(daemon.auth, "intercepted_hosts", return_value=[]),
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon, "bound_bottles", return_value=[]),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.task = asyncio.create_task(daemon.serve(self.path, self.daemon))
        for _ in range(100):
            if self.path.exists():
                break
            await asyncio.sleep(0.01)

    async def asyncTearDown(self) -> None:
        self.task.cancel()

    async def request(self, message: dict) -> dict:
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        writer.write(json.dumps(message).encode() + b"\n")
        reply = json.loads(await reader.readline())
        writer.close()
        return reply

    async def test_ping(self) -> None:
        self.assertEqual(await self.request({"op": "ping"}), {"ok": True})

    async def test_socket_is_owner_only(self) -> None:
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)

    async def test_a_bottles_credentials_reach_its_gateway_and_go_no_further(self) -> None:
        injections = [daemon.egress.Injection(("ci.test",), "Authorization", "Bearer the-real-credential")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=injections) as looked_up:
            reply = await self.request({"op": "bind", "bottle": "b", "features": ["teamcity:server=ci.test"]})
        looked_up.assert_called_once_with(("teamcity:server=ci.test",))
        self.assertNotIn("the-real-credential", json.dumps(reply))
        [binding] = self.daemon.decider.bindings.values()
        self.assertEqual(binding.gateway.injections, tuple(injections))

    async def test_a_bottles_gateway_mints_from_that_bottles_ca(self) -> None:
        with mock.patch.object(daemon.auth, "intercepted_hosts", return_value=["ci.test"]), \
                mock.patch.object(daemon.ca, "server_context") as server_context:
            await self.request({"op": "bind", "bottle": "b", "features": ["teamcity:server=ci.test"]})
            [binding] = self.daemon.decider.bindings.values()
            binding.gateway.certificates("ci.test")
        server_context.assert_called_once_with("b", ("ci.test",), "ci.test")

    async def test_binding_again_reads_the_credentials_again(self) -> None:
        for value in ("Bearer old", "Bearer new"):
            injections = [daemon.egress.Injection(("ci.test",), "Authorization", value)]
            with mock.patch.object(daemon.auth, "injections_for", return_value=injections):
                await self.request({"op": "bind", "bottle": "b", "features": ["teamcity"]})
        [binding] = self.daemon.decider.bindings.values()
        self.assertEqual(binding.gateway.injections[0].value, "Bearer new")

    async def test_shutdown_removes_the_socket_and_keeps_the_tokens(self) -> None:
        await self.request({"op": "bind", "bottle": "b"})
        self.assertEqual(await self.request({"op": "shutdown"}), {"ok": True})
        await asyncio.wait_for(self.task, 2)
        self.assertFalse(self.path.exists())
        self.assertIn("b", json.loads((self.tmp / "tokens.json").read_text()))

    async def test_client_shutdown_when_not_running(self) -> None:
        with mock.patch.object(daemon, "socket_path", return_value=self.path.with_name("absent.sock")):
            self.assertFalse(await asyncio.to_thread(daemon.stop))

    async def test_unknown_op(self) -> None:
        self.assertEqual(await self.request({"op": "nope"}), {"ok": False, "error": "unknown op 'nope'"})

    async def test_second_daemon_exits(self) -> None:
        await asyncio.wait_for(daemon.serve(self.path, Daemon(decider_port=0)), 2)

    async def test_a_taken_decider_port_stops_bottled_saying_so(self) -> None:
        blocker = await asyncio.start_server(lambda r, w: None, "127.0.0.1", 0)
        self.addAsyncCleanup(self._close, blocker)
        taken = blocker.sockets[0].getsockname()[1]
        with self.assertRaisesRegex(BottleError, f"decider can't listen on 127.0.0.1:{taken}"):
            await daemon.serve(self.tmp / "e.sock", Daemon(decider=Decider(self.tmp / "t2.json"), decider_port=taken))

    @staticmethod
    async def _close(server: asyncio.Server) -> None:
        server.close()
        await server.wait_closed()


class RestoreTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        for patcher in (
            mock.patch.object(daemon.auth, "intercepted_hosts", return_value=[]),
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    async def test_binds_the_tokens_it_gave_and_no_others(self) -> None:
        saved = Decider(self.tmp / "tokens.json")
        token = saved.bind("b", gateway=None)
        d = Daemon(decider=Decider(self.tmp / "tokens.json"))
        await d.restore([("b", None, (), ()), ("other", None, (), ())])
        self.assertEqual([binding.bottle for binding in d.decider.bindings.values()], ["b"])
        self.assertEqual(d.decider.tokens["b"], token)

    async def test_one_failure_doesnt_stop_the_rest(self) -> None:
        saved = Decider(self.tmp / "tokens.json")
        saved.bind("bad", gateway=None)
        saved.bind("good", gateway=None)
        d = Daemon(decider=Decider(self.tmp / "tokens.json"))
        real = d._gateway

        def gateway(bottle, *args):
            if bottle == "bad":
                raise OSError("its repo is gone")
            return real(bottle, *args)

        with mock.patch.object(d, "_gateway", side_effect=gateway), self.assertLogs("bottle.daemon", "WARNING"):
            await d.restore([("bad", None, (), ()), ("good", None, (), ())])
        self.assertEqual([binding.bottle for binding in d.decider.bindings.values()], ["good"])
