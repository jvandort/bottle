import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import daemon
from bottle.daemon import Daemon


class SocketPathTest(unittest.TestCase):
    def test_lives_in_bottle_home(self) -> None:
        with mock.patch.dict(os.environ, {"BOTTLE_HOME": "/Users/me/.bottle"}):
            self.assertEqual(daemon.socket_path(), Path("/Users/me/.bottle/bottled.sock"))

    def test_deep_bottle_home_gets_a_short_path(self) -> None:
        deep = "/private/tmp/" + "x" * 120
        with mock.patch.dict(os.environ, {"BOTTLE_HOME": deep}):
            path = daemon.socket_path()
        self.assertLessEqual(len(str(path)), daemon.MAX_SOCKET_PATH)
        self.assertEqual(path.parent, Path(f"/tmp/bottle-{os.getuid()}"))


def free_port() -> int:
    import socket

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class DaemonTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        # Keep the socket path short: macOS caps Unix socket paths at 104 bytes.
        tmp = tempfile.mkdtemp(dir="/tmp")
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        self.path = Path(tmp) / "d.sock"
        self.port = free_port()
        self.daemon = Daemon(port=self.port)
        self.running = getattr(self, "running", [])
        for patcher in (
            mock.patch.object(daemon.runtime, "network_gateway", return_value="127.0.0.1"),
            mock.patch.object(daemon, "running_bottles", side_effect=lambda: self.running),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.task = asyncio.create_task(daemon.serve(self.path, self.daemon))
        for _ in range(100):
            if self.path.exists():
                break
            await asyncio.sleep(0.01)

    async def asyncTearDown(self) -> None:
        for bottle in list(self.daemon.proxies):
            await self.daemon.release(bottle)
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

    async def test_ensure_serves_egress_on_the_gateway(self) -> None:
        reply = await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        self.assertEqual(reply, {"ok": True, "proxy": f"http://127.0.0.1:{self.port}"})
        reader, writer = await asyncio.open_connection("127.0.0.1", self.port)
        writer.write(b"CONNECT 10.0.0.1:443 HTTP/1.1\r\n\r\n")
        self.assertEqual((await reader.readline()).strip(), b"HTTP/1.1 403 Forbidden")
        writer.close()

    async def test_ensure_is_idempotent(self) -> None:
        await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        server = self.daemon.proxies["b"][1]
        await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        self.assertIs(self.daemon.proxies["b"][1], server)

    async def test_ensure_replaces_a_dead_proxy(self) -> None:
        await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        dead = self.daemon.proxies["b"][1]
        dead.close()
        await dead.wait_closed()
        reply = await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        self.assertTrue(reply["ok"])
        self.assertIsNot(self.daemon.proxies["b"][1], dead)

    async def test_a_bottles_credentials_reach_its_proxy_and_go_no_further(self) -> None:
        injections = [daemon.egress.Injection(("ci.test",), "Authorization", "Bearer tok")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=injections) as looked_up:
            await self.request({"op": "ensure", "bottle": "b", "network": "n", "features": ["teamcity:server=ci.test"]})
        looked_up.assert_called_once_with(("teamcity:server=ci.test",))
        self.assertEqual(self.daemon.proxies["b"][0].injections, tuple(injections))

    async def test_a_changed_credential_gets_a_new_proxy(self) -> None:
        first = [daemon.egress.Injection(("ci.test",), "Authorization", "Bearer old")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=first):
            await self.request({"op": "ensure", "bottle": "b", "network": "n", "features": ["teamcity"]})
        server = self.daemon.proxies["b"][1]
        second = [daemon.egress.Injection(("ci.test",), "Authorization", "Bearer new")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=second):
            await self.request({"op": "ensure", "bottle": "b", "network": "n", "features": ["teamcity"]})
        self.assertIsNot(self.daemon.proxies["b"][1], server)
        self.assertEqual(self.daemon.proxies["b"][0].injections, tuple(second))

    async def test_release(self) -> None:
        await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        self.assertEqual(await self.request({"op": "release", "bottle": "b"}), {"ok": True})
        self.assertEqual(self.daemon.proxies, {})

    async def test_port_in_use_is_reported(self) -> None:
        blocker = await asyncio.start_server(lambda r, w: None, "127.0.0.1", self.port)
        self.addAsyncCleanup(self._close, blocker)
        reply = await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        self.assertFalse(reply["ok"])
        self.assertIn(f"can't serve b's egress on 127.0.0.1:{self.port}", reply["error"])

    async def test_shutdown_stops_proxies_and_removes_the_socket(self) -> None:
        await self.request({"op": "ensure", "bottle": "b", "network": "bottle-x"})
        self.assertEqual(await self.request({"op": "shutdown"}), {"ok": True})
        await asyncio.wait_for(self.task, 2)
        self.assertEqual(self.daemon.proxies, {})
        self.assertFalse(self.path.exists())
        with self.assertRaises(OSError):
            await asyncio.open_connection("127.0.0.1", self.port)

    async def test_client_shutdown_when_not_running(self) -> None:
        with mock.patch.object(daemon, "socket_path", return_value=self.path.with_name("absent.sock")):
            self.assertFalse(await asyncio.to_thread(daemon.stop))

    async def test_concurrent_ensures_serve_once(self) -> None:
        replies = await asyncio.gather(*(self.request({"op": "ensure", "bottle": "b", "network": "n"}) for _ in range(5)))
        self.assertTrue(all(r["ok"] for r in replies), replies)
        self.assertEqual(list(self.daemon.proxies), ["b"])

    async def test_unknown_op(self) -> None:
        self.assertEqual(await self.request({"op": "nope"}), {"ok": False, "error": "unknown op 'nope'"})

    async def test_second_daemon_exits(self) -> None:
        await asyncio.wait_for(daemon.serve(self.path, Daemon(port=free_port())), 2)

    @staticmethod
    async def _close(server: asyncio.Server) -> None:
        server.close()
        await server.wait_closed()


class RestoreTest(unittest.IsolatedAsyncioTestCase):
    async def test_restores_running_bottles_on_start(self) -> None:
        tmp = tempfile.mkdtemp(dir="/tmp")
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        path, port = Path(tmp) / "d.sock", free_port()
        d = Daemon(port=port)
        with mock.patch.object(daemon.runtime, "network_gateway", return_value="127.0.0.1"), \
                mock.patch.object(daemon, "running_bottles", return_value=[("b", "bottle-b", None, ())]):
            task = asyncio.create_task(daemon.serve(path, d))
            for _ in range(200):
                if "b" in d.proxies:
                    break
                await asyncio.sleep(0.01)
            self.assertIn("b", d.proxies)
            _, writer = await asyncio.open_connection("127.0.0.1", port)  # serving
            writer.close()
            d.stopping.set()
            await asyncio.wait_for(task, 2)

    async def test_one_failure_doesnt_stop_the_rest(self) -> None:
        d = Daemon(port=free_port())
        gateways = {"bad": OSError("no such network"), "good": "127.0.0.1"}

        def gateway(network):
            value = gateways[network]
            if isinstance(value, Exception):
                raise value
            return value

        with mock.patch.object(daemon.runtime, "network_gateway", side_effect=gateway):
            await d.restore([("x", "bad", None, ()), ("y", "good", None, ())])
        self.assertEqual(list(d.proxies), ["y"])
        await d.release("y")


if __name__ == "__main__":
    unittest.main()
