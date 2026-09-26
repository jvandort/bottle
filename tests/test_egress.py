import asyncio
import ipaddress
import unittest

from bottle import egress
from bottle.egress import EgressProxy, Policy


def ip(address: str):
    return ipaddress.ip_address(address)


class PolicyTest(unittest.TestCase):
    policy = Policy(allow_private=("*.corp.example.com",))

    def test_public_allowed(self) -> None:
        self.assertTrue(self.policy.allows("example.com", ip("93.184.216.34")))
        self.assertTrue(self.policy.allows("example.com", ip("2606:4700::1")))

    def test_private_denied(self) -> None:
        for address in ("10.0.0.5", "172.16.0.2", "192.168.1.1", "100.64.0.1", "fd00::1"):
            with self.subTest(address):
                self.assertFalse(self.policy.allows("example.com", ip(address)))

    def test_private_allowed_for_allowlisted_host(self) -> None:
        self.assertTrue(self.policy.allows("ci.corp.example.com", ip("10.1.2.3")))
        self.assertTrue(self.policy.allows("CI.CORP.EXAMPLE.COM", ip("10.1.2.3")))

    def test_allowlist_needs_a_match(self) -> None:
        self.assertFalse(self.policy.allows("corp.example.com.evil.com", ip("10.1.2.3")))
        self.assertFalse(self.policy.allows("10.1.2.3", ip("10.1.2.3")))

    def test_host_itself_never_allowed(self) -> None:
        for address in ("127.0.0.1", "::1", "169.254.169.254", "fe80::1", "0.0.0.0", "224.0.0.251"):
            with self.subTest(address):
                self.assertFalse(self.policy.allows("ci.corp.example.com", ip(address)))

    def test_ipv4_mapped_addresses_are_unwrapped(self) -> None:
        self.assertFalse(self.policy.allows("example.com", ip("::ffff:192.168.1.1")))
        self.assertFalse(self.policy.allows("ci.corp.example.com", ip("::ffff:127.0.0.1")))


class LoopbackPolicy(Policy):
    """Treats 127.0.0.1 as public, so tests can reach their servers there."""

    def allows(self, host, address) -> bool:
        return address == ip("127.0.0.1") or super().allows(host, address)


HOSTS = {"upstream.test": ["127.0.0.1"], "private.test": ["10.0.0.5"], "mixed.test": ["10.0.0.5", "127.0.0.1"]}


async def fake_resolve(host: str, port: int) -> list[str]:
    try:
        return [str(ip(host))]
    except ValueError:
        pass
    if host not in HOSTS:
        raise OSError("unknown host")
    return HOSTS[host]


class ProxyTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.received = asyncio.Queue()
        upstream = await asyncio.start_server(self.upstream, "127.0.0.1", 0)
        self.addAsyncCleanup(self.close, upstream)
        self.upstream_port = upstream.sockets[0].getsockname()[1]

        proxy = await EgressProxy("test", LoopbackPolicy(), fake_resolve).start("127.0.0.1", 0)
        self.addAsyncCleanup(self.close, proxy)
        self.proxy_port = proxy.sockets[0].getsockname()[1]

    @staticmethod
    async def close(server: asyncio.Server) -> None:
        server.close()
        await server.wait_closed()

    async def upstream(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """Echoes raw bytes, or answers an HTTP request after recording its head."""
        first = await reader.read(64 * 1024)
        if first.startswith(b"GET "):
            await self.received.put(first)
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
        else:
            while first:
                writer.write(first)
                await writer.drain()
                first = await reader.read(64 * 1024)
        writer.close()

    async def send(self, request: bytes) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        reader, writer = await asyncio.open_connection("127.0.0.1", self.proxy_port)
        self.addCleanup(writer.close)
        writer.write(request)
        return reader, writer

    async def status(self, request: bytes) -> bytes:
        reader, _ = await self.send(request)
        return (await asyncio.wait_for(reader.readline(), 5)).strip()

    async def test_connect_tunnels_bytes(self) -> None:
        reader, writer = await self.send(f"CONNECT upstream.test:{self.upstream_port} HTTP/1.1\r\n\r\n".encode())
        self.assertEqual(await reader.readuntil(b"\r\n\r\n"), b"HTTP/1.1 200 Connection Established\r\n\r\n")
        writer.write(b"ping")
        self.assertEqual(await asyncio.wait_for(reader.readexactly(4), 5), b"ping")

    async def test_connect_forwards_data_sent_with_the_request(self) -> None:
        reader, _ = await self.send(f"CONNECT upstream.test:{self.upstream_port} HTTP/1.1\r\n\r\nearly".encode())
        await reader.readuntil(b"\r\n\r\n")
        self.assertEqual(await asyncio.wait_for(reader.readexactly(5), 5), b"early")

    async def test_plain_http_is_rewritten_for_the_origin(self) -> None:
        reader, _ = await self.send(
            f"GET http://upstream.test:{self.upstream_port}/pkg?x=1 HTTP/1.1\r\n"
            f"Host: upstream.test:{self.upstream_port}\r\nProxy-Connection: keep-alive\r\n"
            f"Proxy-Authorization: Basic secret\r\nAccept: */*\r\n\r\n".encode()
        )
        response = await asyncio.wait_for(reader.read(), 5)
        head = (await self.received.get()).decode()

        self.assertTrue(response.endswith(b"hello"))
        self.assertTrue(head.startswith("GET /pkg?x=1 HTTP/1.1\r\n"))
        self.assertIn("Accept: */*", head)
        self.assertIn("Connection: close", head)
        self.assertNotIn("Proxy-", head)

    async def test_plain_http_without_host_header_gets_one(self) -> None:
        reader, _ = await self.send(f"GET http://upstream.test:{self.upstream_port}/ HTTP/1.0\r\n\r\n".encode())
        await asyncio.wait_for(reader.read(), 5)
        self.assertIn(f"Host: upstream.test:{self.upstream_port}", (await self.received.get()).decode())

    async def test_private_destination_denied(self) -> None:
        self.assertEqual(await self.status(b"CONNECT private.test:443 HTTP/1.1\r\n\r\n"), b"HTTP/1.1 403 Forbidden")

    async def test_private_ip_literal_denied(self) -> None:
        with self.assertLogs("bottle.egress") as logs:
            status = await self.status(b"CONNECT 192.168.1.1:22 HTTP/1.1\r\n\r\n")
            await asyncio.sleep(0.05)
        self.assertEqual(status, b"HTTP/1.1 403 Forbidden")
        self.assertIn("denied", logs.output[-1])

    async def test_denied_addresses_are_skipped(self) -> None:
        # mixed.test resolves to a private address first; it must be skipped, not connected to.
        status = await self.status(f"CONNECT mixed.test:{self.upstream_port} HTTP/1.1\r\n\r\n".encode())
        self.assertEqual(status, b"HTTP/1.1 200 Connection Established")

    async def test_unresolvable_host(self) -> None:
        self.assertEqual(await self.status(b"CONNECT nowhere.test:443 HTTP/1.1\r\n\r\n"), b"HTTP/1.1 502 Bad Gateway")

    async def test_refused_connection(self) -> None:
        closed = await asyncio.start_server(lambda r, w: None, "127.0.0.1", 0)
        port = closed.sockets[0].getsockname()[1]
        await self.close(closed)
        self.assertEqual(await self.status(f"CONNECT upstream.test:{port} HTTP/1.1\r\n\r\n".encode()), b"HTTP/1.1 502 Bad Gateway")

    async def test_malformed_requests(self) -> None:
        for request in (
            b"nonsense\r\n\r\n",
            b"GET /origin-form HTTP/1.1\r\nHost: x\r\n\r\n",
            b"GET https://example.com/ HTTP/1.1\r\n\r\n",
            b"CONNECT no-port HTTP/1.1\r\n\r\n",
        ):
            with self.subTest(request):
                self.assertEqual(await self.status(request), b"HTTP/1.1 400 Bad Request")

    async def test_oversized_headers(self) -> None:
        request = b"CONNECT upstream.test:1 HTTP/1.1\r\nX: " + b"a" * egress.MAX_HEADER_BYTES + b"\r\n\r\n"
        self.assertEqual(await self.status(request), b"HTTP/1.1 431 Request Header Fields Too Large")

    async def test_logs_each_connection(self) -> None:
        with self.assertLogs("bottle.egress") as logs:
            reader, _ = await self.send(f"GET http://upstream.test:{self.upstream_port}/ HTTP/1.1\r\n\r\n".encode())
            await asyncio.wait_for(reader.read(), 5)
            await asyncio.sleep(0.05)
        self.assertRegex(logs.output[-1], rf"test GET upstream.test:{self.upstream_port} -> 127.0.0.1 ok up=\d+ down=\d+")


if __name__ == "__main__":
    unittest.main()
