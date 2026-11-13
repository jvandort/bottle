import asyncio
import dataclasses
import ssl
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import egress, host
from bottle.egress import CREDENTIAL, GIT, SSH, Flow, Gateway, Redirect, Refused
from tests.test_egress import Upstream, fake_resolve


class DecideTest(unittest.IsolatedAsyncioTestCase):
    """Flows a machine routed here, judged by the address the bottle connected to and a name it can't vouch for."""

    def setUp(self) -> None:
        self.injection = egress.Injection(host="api.test", header="Authorization", value="Bearer real", port=443)
        self.resolved = {"api.test": ["93.184.216.34"], "ci.corp.test": ["10.0.0.5"], "here.test": ["127.0.0.1"]}

        async def resolve(name, port):
            if name not in self.resolved:
                raise OSError("unknown host")
            return self.resolved[name]

        self.proxy = Gateway("t", services=object(), injections=(self.injection,), resolve=resolve)

    async def decide(self, address: str, port: int = 443, name: str | None = None, tls: bool = True):
        return await self.proxy.decide(Flow(address, port, name, unwrapped=tls))

    async def refused(self, *args, **kwargs) -> None:
        with self.assertRaises(Refused):
            await self.decide(*args, **kwargs)

    async def test_bottle_host_is_the_git_service_on_port_80_only(self) -> None:
        self.assertEqual(await self.decide(host.ADDRESS, 80, tls=False), Redirect(GIT))
        await self.refused(host.ADDRESS, 22)

    async def test_the_rest_of_test_net_1_is_ssh_to_bottles_on_port_22_only(self) -> None:
        await self.refused("192.0.2.2", 22)   # nothing to look up the grant
        self.proxy.ssh = lambda address: None
        for address in ("192.0.2.2", "192.0.2.200"):
            self.assertEqual(await self.decide(address, 22, tls=False), Redirect(SSH))
            await self.refused(address, 80)

    async def test_bottle_host_without_services_is_refused(self) -> None:
        self.proxy.services = None
        await self.refused(host.ADDRESS, 80)

    async def test_a_credentials_host_is_served_whatever_address_the_bottle_connected_to(self) -> None:
        decision = await self.decide("10.9.9.9", name="api.test")
        self.assertEqual(decision, Redirect(CREDENTIAL, self.injection, ("93.184.216.34",)))

    async def test_anything_else_is_no_service(self) -> None:
        """What a policy out of date routes here: another port, no TLS, another name, no name."""
        for args in (("93.184.216.34", 8443, "api.test"), ("93.184.216.34", 443, "api.test", False),
                     ("93.184.216.34", 443, "example.com"), ("93.184.216.34",)):
            with self.subTest(args), self.assertRaises(Refused) as refused:
                await self.decide(*args)
            self.assertEqual(refused.exception.outcome, "not-a-service")

    async def test_a_credentials_host_at_this_machine_is_refused(self) -> None:
        self.injection = dataclasses.replace(self.injection, host="here.test")
        self.proxy.injections = (self.injection,)
        await self.refused("10.9.9.9", name="here.test")

    async def test_a_credentials_host_at_a_private_address_is_reached(self) -> None:
        self.injection = dataclasses.replace(self.injection, host="ci.corp.test")
        self.proxy.injections = (self.injection,)
        self.assertEqual((await self.decide("10.9.9.9", name="ci.corp.test")).addresses, ("10.0.0.5",))

    async def test_a_credentials_host_that_doesnt_resolve_is_refused(self) -> None:
        self.injection = dataclasses.replace(self.injection, host="gone.test")
        self.proxy.injections = (self.injection,)
        await self.refused("10.9.9.9", name="gone.test")


class ServeTest(unittest.IsolatedAsyncioTestCase):
    """Flows served as services.py serves what a machine routes here: TLS taken off by bottle-cli, the name said."""

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

        self.services = FakeServices()
        injection = egress.Injection(
            host="upstream.test", header="Authorization", value="Bearer real-token", port=self.upstream_port,
            standin="stand-in", credential="real-token",
        )
        self.proxy = Gateway("test", services=self.services, injections=(injection,), resolve=fake_resolve,
                             reachable=lambda ip: True)
        routed = await asyncio.start_server(self.routed, "127.0.0.1", 0)
        self.addAsyncCleanup(Upstream.close, routed)
        self.routed_port = routed.sockets[0].getsockname()[1]

    upstream = Upstream.upstream

    async def routed(self, reader, writer) -> None:
        address, port, name, tls = self.flow
        await self.proxy.serve(Flow(address, port, name, unwrapped=tls), reader, writer)

    async def connect(self, address: str, port: int, name: str | None = None, tls: bool = False):
        self.flow = (address, port, name, tls)
        return await asyncio.open_connection("127.0.0.1", self.routed_port)

    async def test_a_credentials_flow_gets_the_credential_in_plain_http(self) -> None:
        reader, writer = await self.connect("203.0.113.5", self.upstream_port, "upstream.test", tls=True)
        writer.write(b"GET /api HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Bearer stand-in\r\n\r\n")
        response = await asyncio.wait_for(reader.read(), 5)
        head = (await asyncio.wait_for(self.received.get(), 5)).decode()
        self.assertTrue(response.endswith(b"hello"))
        self.assertIn("Authorization: Bearer real-token", head)
        self.assertNotIn("stand-in", head)

    async def sshd_says_hello_and_echoes(self) -> None:
        self.asked = []

        async def command(address):
            self.asked.append(address)
            return ["sh", "-c", "echo SSH-2.0-there; cat"]

        self.proxy.ssh = command

    async def test_a_flow_to_a_granted_address_is_joined_to_sshd_there(self) -> None:
        await self.sshd_says_hello_and_echoes()
        reader, writer = await self.connect("192.0.2.3", 22)
        writer.write(b"SSH-2.0-client\r\n")
        writer.write_eof()
        self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"SSH-2.0-there\nSSH-2.0-client\r\n")
        self.assertEqual(self.asked, ["192.0.2.3"])

    async def test_a_flow_to_an_address_granted_nothing_is_closed(self) -> None:
        async def none(address):
            raise Refused(None, f"test may ssh to nothing at {address}", "no-ssh")

        self.proxy.ssh = none
        reader, writer = await self.connect("192.0.2.3", 22)
        self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"")

    async def test_a_flow_for_no_service_is_closed(self) -> None:
        with self.assertLogs("bottle.egress", "WARNING"):
            reader, writer = await self.connect("203.0.113.5", 443, "elsewhere.test", tls=True)
            self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"")
        writer.close()

    async def test_git_is_served(self) -> None:
        reader, writer = await self.connect(host.ADDRESS, 80)
        writer.write(b"POST /git/agent/git-receive-pack HTTP/1.1\r\nHost: bottle.host\r\nContent-Length: 4\r\n\r\npack")
        self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"HTTP/1.1 200 OK\r\n\r\nserved")
        request = self.services.requests[0]
        self.assertEqual((request.method, request.path, request.body), ("POST", "/git/agent/git-receive-pack", b"pack"))

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
