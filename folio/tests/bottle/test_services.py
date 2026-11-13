import asyncio
import ipaddress
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from folio.bottle import cache, daemon, host, services
from folio.bottle.daemon import Daemon


def header(address: str, port: int, name: str | None = None, tls: bool = False, machine: str | None = "b-m") -> bytes:
    """What bottle-cli writes before a routed connection's bytes (agent-client's net.rs, proxy_header)."""
    ip = ipaddress.ip_address(address)
    body = bytes(len(ip.packed)) + ip.packed + bytes(2) + port.to_bytes(2, "big")
    for kind, value in ((services.PP2_TYPE_AUTHORITY, name.encode() if name else None),
                        (services.PP2_TYPE_SSL, bytes([services.PP2_CLIENT_SSL, 0, 0, 0, 0]) if tls else None),
                        (services.PP2_TYPE_MACHINE, machine.encode() if machine else None)):
        if value is not None:
            body += bytes([kind]) + len(value).to_bytes(2, "big") + value
    family = services.TCP_OVER_IPV4 if ip.version == 4 else services.TCP_OVER_IPV6
    return services.SIGNATURE + bytes([services.VERSION_2_PROXY, family]) + len(body).to_bytes(2, "big") + body


class HeaderTest(unittest.IsolatedAsyncioTestCase):
    async def read(self, data: bytes) -> services.Header:
        reader = asyncio.StreamReader()
        reader.feed_data(data)
        reader.feed_eof()
        return await services.read_header(reader)

    async def test_what_bottle_cli_says_of_a_connection(self) -> None:
        self.assertEqual(await self.read(header("1.2.3.4", 443, "api.example", tls=True, machine="gpu")),
                         services.Header("1.2.3.4", 443, "api.example", True, "gpu"))
        self.assertEqual(await self.read(header("192.0.2.1", 80)), services.Header("192.0.2.1", 80, None, False, "b-m"))
        self.assertEqual((await self.read(header("2001:db8::1", 22))).address, "2001:db8::1")

    async def test_anything_else_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            await self.read(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        whole = header("1.2.3.4", 443, "api.example")
        for end in (0, 10, 20, len(whole) - 1):
            with self.subTest(end), self.assertRaises(asyncio.IncompleteReadError):
                await self.read(whole[:end])


class NetworkTest(unittest.TestCase):
    def test_a_machines_policy_routes_bottle_host_ssh_and_credentials_here_and_says_what_it_caches(self) -> None:
        policy = services.network([("api.example", 443), ("ci.example", 8111), ("api.example", 443)],
                                  [cache.Cached("deb.example", include=("/debian/*.deb",), ports=(80,), immutable=True)],
                                  sockets={"host": "/h.sock", "ssh": "/s.sock", "credentials": "/c.sock"})
        self.assertEqual(policy["routes"], [
            {"address": host.ADDRESS, "port": 80, "socket": "/h.sock"},
            {"network": host.SSH_NETWORK, "port": host.SSH_PORT, "socket": "/s.sock"},
            {"host": "api.example", "port": 443, "socket": "/c.sock"},
            {"host": "ci.example", "port": 8111, "socket": "/c.sock"},
        ])
        self.assertEqual(policy["cache"], [{"host": "deb.example", "include": ["/debian/*.deb"], "exclude": [],
                                            "ports": [80], "immutable": True}])
        self.assertEqual(policy["allow_private"], [])
        # Made without sockets, each route says its service, for the sockets where it's made.
        self.assertEqual(services.network([], [])["routes"][0]["service"], "host")
        given = services.with_sockets(services.network([], []), {"host": "/h", "ssh": "/s", "credentials": "/c"})
        self.assertEqual(given["routes"][1], {"network": host.SSH_NETWORK, "port": host.SSH_PORT, "socket": "/s"})


class ServicesTest(unittest.IsolatedAsyncioTestCase):
    """bottled's services socket, as bottle-cli meets it: a PROXY header, then the connection."""

    async def asyncSetUp(self) -> None:
        tmp = tempfile.mkdtemp(dir="/tmp")
        self.addCleanup(shutil.rmtree, tmp, True)
        self.tmp = Path(tmp)
        for patcher in (
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon, "bound_bottles", return_value=[]),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.socket = self.tmp / "host.sock"
        self.daemon = Daemon(services_paths={n: self.tmp / f"{n}.sock" for n in daemon.SERVICES})
        self.task = asyncio.create_task(daemon.serve(self.tmp / "d.sock", self.daemon))
        for _ in range(100):
            if self.socket.exists():
                break
            await asyncio.sleep(0.01)
        await self.daemon.bind("b", "b-m")

    async def asyncTearDown(self) -> None:
        self.task.cancel()

    async def routed(self, prelude: bytes, then: bytes = b"") -> bytes:
        reader, writer = await asyncio.open_unix_connection(str(self.socket))
        writer.write(prelude + then)
        await writer.drain()
        try:
            return await asyncio.wait_for(reader.read(), 5)
        finally:
            writer.close()

    async def test_bottle_host_is_answered_for_the_bottle_its_machine_is(self) -> None:
        answer = await self.routed(header(host.ADDRESS, 80, "bottle.host"),
                                   b"GET /git/nothing/info/refs HTTP/1.1\r\nHost: bottle.host\r\n\r\n")
        # No repo, so no remotes: bottle.host itself answers that there's nothing there.
        self.assertTrue(answer.startswith(b"HTTP/1.1 404"), answer)

    async def test_a_machine_no_bottle_is_gets_nothing(self) -> None:
        with self.assertLogs("bottle.services", "WARNING"):
            self.assertEqual(await self.routed(header(host.ADDRESS, 80, machine="stranger"), b"GET / HTTP/1.1\r\n\r\n"),
                             b"")

    async def test_what_isnt_a_service_gets_nothing(self) -> None:
        # Routed here by a policy out of date: an address bottled serves nothing at.
        self.assertEqual(await self.routed(header("93.184.216.34", 443, "example.com", tls=True)), b"")

    async def test_released_its_machine_is_no_bottles(self) -> None:
        await self.daemon.bind("other", "other-m")   # one left, so folio serve stays
        await self.daemon.release("b")
        with self.assertLogs("bottle.services", "WARNING"):
            self.assertEqual(await self.routed(header(host.ADDRESS, 80), b"GET / HTTP/1.1\r\n\r\n"), b"")


if __name__ == "__main__":
    unittest.main()
