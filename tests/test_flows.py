import asyncio
import ssl
import subprocess
import tempfile
import functools
import unittest
from pathlib import Path
from unittest import mock

from bottle import ca, egress, host
from bottle.egress import CREDENTIAL, GIT, OPENING, Direct, Flow, Gateway, Policy, Redirect, Refused
from tests.support import client_hello
from tests.test_egress import LoopbackPolicy, Upstream, fake_resolve


class NameTest(unittest.TestCase):
    def test_the_sni_of_a_client_hello(self) -> None:
        self.assertEqual(egress.name_in(client_hello("API.Example.com")), "api.example.com")

    def test_a_client_hello_without_sni(self) -> None:
        self.assertIsNone(egress.name_in(client_hello(None)))

    def test_any_cut_of_a_client_hello_is_survived(self) -> None:
        hello = client_hello("api.example.com")
        for end in range(len(hello)):
            with self.subTest(end):
                self.assertIn(egress.sni(hello[:end]), (None, "api.example.com"))

    def test_lengths_that_point_past_the_record(self) -> None:
        hello = bytearray(client_hello("api.example.com"))
        for at in range(len(hello)):
            corrupt = bytearray(hello)
            corrupt[at] = 0xFF
            egress.sni(bytes(corrupt))  # never raises

    def test_the_host_of_an_http_request(self) -> None:
        for head, name in (
            (b"GET / HTTP/1.1\r\nHost: Example.COM\r\n\r\n", "example.com"),
            (b"GET / HTTP/1.1\r\nhost: example.com:8080\r\n\r\n", "example.com"),
            (b"GET / HTTP/1.1\r\nHost: [::1]:80\r\n\r\n", "::1"),
            (b"GET /git/x HTTP/1.1\r\nUser-Agent: git\r\nHost: bottle.host\r\nAccept: */*\r\n\r\n", "bottle.host"),
        ):
            with self.subTest(head):
                self.assertEqual(egress.name_in(head), name)

    def test_no_host(self) -> None:
        for head in (
            b"GET / HTTP/1.1\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: exam",  # cut off before the line ends
            b"SSH-2.0-OpenSSH_9.6\r\n",
            b"GET / HTTP/1.1\r\nHost: bad name\r\n\r\n",
            b"",
        ):
            with self.subTest(head):
                self.assertIsNone(egress.name_in(head))


class DecideTest(unittest.IsolatedAsyncioTestCase):
    """Flows to the address the bottle connected to, judged by that address and a name it can't vouch for."""

    def setUp(self) -> None:
        self.injection = egress.Injection(host="api.test", header="Authorization", value="Bearer real", port=443)
        self.resolved = {"api.test": ["93.184.216.34"], "ci.corp.test": ["10.0.0.5"], "v6.corp.test": ["fd00:0:0::5"]}

        async def resolve(name, port):
            if name not in self.resolved:
                raise OSError("unknown host")
            return self.resolved[name]

        self.proxy = Gateway(
            "t", Policy(allow_private=("*.corp.test",)), resolve, services=object(),
            injections=(self.injection,), certificates=lambda name: None,
        )

    async def decide(self, address: str, port: int = 443, name: str | None = None):
        first = client_hello(name) if name and port == 443 else (
            f"GET / HTTP/1.1\r\nHost: {name}\r\n\r\n".encode() if name else b"no name in these")
        return await self.proxy.decide(Flow(address, port, first))

    async def refused(self, *args, **kwargs) -> None:
        with self.assertRaises(Refused):
            await self.decide(*args, **kwargs)

    async def test_a_clienthello_not_all_shown_is_read_by_bottled_on_a_credentials_port(self) -> None:
        hello = client_hello("api.test")
        for first in (b"", hello[:10]):
            with self.subTest(first=first):
                self.assertEqual(await self.proxy.decide(Flow("93.184.216.34", 443, first)), Redirect(OPENING))
        # Once bottled has read what it's going to, it decides on that.
        self.assertEqual(await self.proxy.decide(Flow("93.184.216.34", 443, b""), opening_read=True),
                         Direct(("93.184.216.34",)))
        # No credential uses the port, or the address is the host's own: nothing to wait for.
        self.assertEqual(await self.proxy.decide(Flow("93.184.216.34", 8443, b"")), Direct(("93.184.216.34",)))
        with self.assertRaises(Refused):
            await self.proxy.decide(Flow("127.0.0.1", 443, b""))

    async def test_a_public_address_goes_direct(self) -> None:
        self.assertEqual(await self.decide("93.184.216.34"), Direct(("93.184.216.34",)))

    async def test_a_public_ipv6_address_goes_direct(self) -> None:
        self.assertEqual(await self.decide("2606:4700::1"), Direct(("2606:4700::1",)))

    async def test_ipv6_is_judged_as_ipv4_is(self) -> None:
        for address in ("fd00::5", "fe80::1", "::1", "ff02::1", "::ffff:10.0.0.5", "::ffff:127.0.0.1"):
            with self.subTest(address):
                await self.refused(address)

    async def test_a_private_ipv6_address_goes_direct_for_an_allowed_name_that_leads_there(self) -> None:
        # Spelled otherwise than the lookup has it: the same address.
        self.assertEqual(await self.decide("fd00::5", 80, name="v6.corp.test"), Direct(("fd00::5",)))
        await self.refused("fd00::6", 80, name="v6.corp.test")

    async def test_bottle_host_is_the_git_service_on_port_80_only(self) -> None:
        self.assertEqual(await self.decide(host.ADDRESS, 80), Redirect(GIT))
        await self.refused(host.ADDRESS, 22)

    async def test_bottle_host_without_services_is_refused(self) -> None:
        self.proxy.services = None
        await self.refused(host.ADDRESS, 80)

    async def test_a_credentials_host_is_redirected_whatever_address_the_bottle_connected_to(self) -> None:
        decision = await self.decide("10.9.9.9", name="api.test")
        self.assertEqual(decision, Redirect(CREDENTIAL, self.injection, ("93.184.216.34",)))

    async def test_a_credentials_name_on_another_port_is_not_redirected(self) -> None:
        self.assertEqual(await self.decide("93.184.216.34", 8443, name="api.test"), Direct(("93.184.216.34",)))

    async def test_the_host_itself_is_refused(self) -> None:
        for address in ("127.0.0.1", "169.254.169.254", "0.0.0.0", "224.0.0.251"):
            with self.subTest(address):
                await self.refused(address, name="ci.corp.test")

    async def test_a_private_address_is_refused(self) -> None:
        await self.refused("10.0.0.5")
        await self.refused("10.0.0.5", name="example.com")

    async def test_a_private_address_goes_direct_for_an_allowed_name_that_leads_there(self) -> None:
        self.assertEqual(await self.decide("10.0.0.5", name="ci.corp.test"), Direct(("10.0.0.5",)))
        self.assertEqual(await self.decide("10.0.0.5", 80, name="ci.corp.test"), Direct(("10.0.0.5",)))

    async def test_an_allowed_name_sent_to_another_private_address_is_refused(self) -> None:
        await self.refused("192.168.1.1", name="ci.corp.test")

    async def test_an_allowed_name_that_doesnt_resolve_is_refused(self) -> None:
        await self.refused("10.0.0.6", name="gone.corp.test")


class ServeTest(unittest.IsolatedAsyncioTestCase):
    """Redirected flows, whose first bytes the decider has already read off the connection."""

    async def asyncSetUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        certificate, key = Path(tmp.name) / "cert.pem", Path(tmp.name) / "key.pem"
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-keyout", str(key), "-out", str(certificate), "-subj", "/CN=upstream.test",
             "-addext", "subjectAltName=DNS:upstream.test"],
            check=True, capture_output=True,
        )
        serving = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        serving.load_cert_chain(certificate, key)
        self.received = asyncio.Queue()
        upstream = await asyncio.start_server(self.upstream, "127.0.0.1", 0, ssl=serving)
        self.addAsyncCleanup(Upstream.close, upstream)
        self.upstream_port = upstream.sockets[0].getsockname()[1]
        trusting = ssl.create_default_context(cafile=str(certificate))
        patcher = mock.patch.object(egress, "_tls_context", lambda: trusting)
        patcher.start()
        self.addCleanup(patcher.stop)
        home = mock.patch.dict("os.environ", {"BOTTLE_HOME": str(Path(tmp.name) / "home")})
        home.start()
        self.addCleanup(home.stop)
        self.bottle_trusts = ssl.create_default_context(cadata=ca.certificate("test", ("upstream.test",)))

        self.services = FakeServices()
        injection = egress.Injection(
            host="upstream.test", header="Authorization", value="Bearer real-token", port=self.upstream_port,
            standin="stand-in", credential="real-token",
        )
        self.proxy = Gateway(
            "test", LoopbackPolicy(), fake_resolve, services=self.services, injections=(injection,),
            certificates=functools.partial(ca.server_context, "test", ("upstream.test",)),
        )
        self.decisions = []
        decider = await asyncio.start_server(self.decider, "127.0.0.1", 0)
        self.addAsyncCleanup(Upstream.close, decider)
        self.decider_port = decider.sockets[0].getsockname()[1]

    upstream = Upstream.upstream

    async def decider(self, reader, writer) -> None:
        """What the decider will do: read the first bytes, decide, and serve the redirect."""
        address, port = self.connected_to
        first = await reader.read(self.first_size)
        flow = Flow(address, port, first)
        decision = await self.proxy.decide(flow)
        self.decisions.append(decision)
        await self.proxy.serve(flow, decision, reader, writer)

    async def connect(self, address: str, port: int, first_size: int = 64 * 1024, tls: bool = False):
        self.connected_to, self.first_size = (address, port), first_size
        if tls:
            return await asyncio.open_connection(
                "127.0.0.1", self.decider_port, ssl=self.bottle_trusts, server_hostname="upstream.test")
        return await asyncio.open_connection("127.0.0.1", self.decider_port)

    async def test_a_credentials_host_is_served_over_tls_begun_in_the_first_bytes(self) -> None:
        # The bottle connected to some address; the credential goes to upstream.test's own.
        reader, writer = await self.connect("203.0.113.5", self.upstream_port, tls=True)
        writer.write(b"GET /api HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Bearer stand-in\r\n\r\n")
        response = await asyncio.wait_for(reader.read(), 5)
        head = (await asyncio.wait_for(self.received.get(), 5)).decode()
        self.assertTrue(response.endswith(b"hello"))
        self.assertIn("Authorization: Bearer real-token", head)
        self.assertNotIn("stand-in", head)
        self.assertEqual(self.decisions[0].service, CREDENTIAL)

    async def test_a_credentials_flow_refused_before_its_tls_gets_no_plaintext_answer(self) -> None:
        def no_certificate(name):
            raise RuntimeError("no CA")

        self.proxy.certificates = no_certificate
        reader, writer = await self.connect("203.0.113.5", self.upstream_port)
        writer.write(client_hello("upstream.test"))
        self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"")

    async def test_git_is_served_from_a_head_in_the_first_bytes(self) -> None:
        reader, writer = await self.connect(host.ADDRESS, 80)
        writer.write(b"POST /git/agent/git-receive-pack HTTP/1.1\r\nHost: bottle.host\r\nContent-Length: 4\r\n\r\npack")
        self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"HTTP/1.1 200 OK\r\n\r\nserved")
        request = self.services.requests[0]
        self.assertEqual((request.method, request.path, request.body), ("POST", "/git/agent/git-receive-pack", b"pack"))

    async def test_git_with_its_head_split_beyond_the_first_bytes(self) -> None:
        reader, writer = await self.connect(host.ADDRESS, 80, first_size=10)
        writer.write(b"GET /git/agent/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n")
        await asyncio.wait_for(reader.read(), 5)
        request = self.services.requests[0]
        self.assertEqual((request.path, request.query), ("/git/agent/info/refs", "service=git-upload-pack"))

    async def test_git_wants_an_origin_form_request(self) -> None:
        reader, writer = await self.connect(host.ADDRESS, 80)
        writer.write(b"GET http://bottle.host/git/x HTTP/1.1\r\nHost: bottle.host\r\n\r\n")
        self.assertTrue((await asyncio.wait_for(reader.read(), 5)).startswith(b"HTTP/1.1 400"))
        self.assertEqual(self.services.requests, [])


class FakeServices:
    def __init__(self) -> None:
        self.requests = []

    async def handle(self, request, writer) -> str:
        self.requests.append(request)
        writer.write(b"HTTP/1.1 200 OK\r\n\r\nserved")
        await writer.drain()
        return "ok"
