import asyncio
import base64
import functools
import ipaddress
import shutil
import ssl
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import ca, egress
from bottle.egress import Direct, Flow, Gateway, Policy, Refused
from tests.support import client_hello

# The address a bottle connected to for the credential's host: never where it goes.
CONNECTED_TO = "203.0.113.5"


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

    def allows(self, host, address, private=False) -> bool:
        return address == ip("127.0.0.1") or super().allows(host, address, private)


HOSTS = {"upstream.test": ["127.0.0.1"], "elsewhere.test": ["127.0.0.1"], "private.test": ["10.0.0.5"], "mixed.test": ["10.0.0.5", "127.0.0.1"],
         "metadata.test": ["169.254.169.254"]}


async def fake_resolve(host: str, port: int) -> list[str]:
    try:
        return [str(ip(host))]
    except ValueError:
        pass
    if host not in HOSTS:
        raise OSError("unknown host")
    return HOSTS[host]


class Upstream:
    """A server to reach, and closing the servers a test starts."""

    @staticmethod
    async def close(server: asyncio.Server) -> None:
        server.close()
        await server.wait_closed()

    async def upstream(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """Echoes raw bytes, or answers an HTTP request after recording its head."""
        first = await reader.read(64 * 1024)
        if first.startswith((b"GET ", b"POST ")):
            await self.received.put(first)
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
        else:
            while first:
                writer.write(first)
                await writer.drain()
                first = await reader.read(64 * 1024)
        writer.close()


class StatusTest(unittest.TestCase):
    def test_the_status_code(self) -> None:
        self.assertEqual(egress._status(b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\n"), "401")

    def test_an_interim_response_gives_way_to_the_final_one(self) -> None:
        self.assertEqual(egress._status(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\n"), "201")

    def test_an_interim_response_alone_is_all_there_is_to_report(self) -> None:
        self.assertEqual(egress._status(b"HTTP/1.1 100 Continue\r\n"), "100")

    def test_not_http(self) -> None:
        self.assertIsNone(egress._status(b""))
        self.assertIsNone(egress._status(b"SSH-2.0-OpenSSH_9.6\r\n"))


@unittest.skipUnless(shutil.which("openssl"), "needs openssl to make a certificate")
class InjectionTest(unittest.IsolatedAsyncioTestCase):
    """What bottled attaches for the bottle, and where.

    The bottle talks HTTPS to an address, as to anywhere, and smolvm hands
    bottled the flow with its ClientHello. For an injected host bottled
    answers the TLS handshake itself, with a certificate from the egress CA
    (the bottle trusts it), and the upstream is a TLS server, because that is
    the only kind an injected request has.
    """

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

        # The proxy trusts the upstream's certificate; the bottle trusts the egress CA.
        trusting = ssl.create_default_context(cafile=str(certificate))
        patcher = mock.patch.object(egress, "_tls_context", lambda: trusting)
        patcher.start()
        self.addCleanup(patcher.stop)
        home = mock.patch.dict("os.environ", {"BOTTLE_HOME": str(Path(tmp.name) / "home")})
        home.start()
        self.addCleanup(home.stop)
        self.bottle_trusts = ssl.create_default_context(cadata=ca.certificate("test", ("upstream.test",)))

        self.asked: list[tuple[str, bool]] = []
        injection = egress.Injection(
            host="upstream.test", header="Authorization", value="Bearer real-token", port=self.upstream_port,
            standin="stand-in", credential="real-token",
        )
        self.gateway = Gateway(
            "test", self.recording_policy(), fake_resolve, injections=(injection,),
            certificates=functools.partial(ca.server_context, "test", ("upstream.test",)),
        )
        server = await asyncio.start_server(self.decider, "127.0.0.1", 0)
        self.addAsyncCleanup(Upstream.close, server)
        self.port = server.sockets[0].getsockname()[1]

    # How much of the flow smolvm shows the decider: what arrived (None), or just
    # this many bytes, as when the guest's ClientHello comes after smolvm stops waiting.
    shown: int | None = None
    connected_to = CONNECTED_TO

    async def decider(self, reader, writer) -> None:
        """What the decider does with a flow smolvm hands it: decide, and serve it if it's bottled's."""
        first = await reader.read(64 * 1024) if self.shown is None else await reader.readexactly(self.shown)
        flow = Flow(self.connected_to, self.connected_port, first)
        try:
            decision = await self.gateway.decide(flow)
        except Refused:
            writer.close()
            return
        await self.gateway.serve(flow, decision, reader, writer)

    def recording_policy(self) -> Policy:
        asked = self.asked

        class Recording(LoopbackPolicy):
            def allows(self, host, address, private=False):
                asked.append((host, private))
                return super().allows(host, address, private)

        return Recording()

    upstream = Upstream.upstream

    async def tunnel(self, host: str = "upstream.test", port: int | None = None,
                     trust: ssl.SSLContext | None = None) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        """What a bottle's HTTPS client does: TLS to the host's address, which smolvm hands bottled."""
        self.connected_port = port or self.upstream_port
        reader, writer = await asyncio.wait_for(asyncio.open_connection(
            "127.0.0.1", self.port, ssl=trust or self.bottle_trusts, server_hostname=host), 5)
        self.addCleanup(writer.close)
        return reader, writer

    async def send(self, request: bytes) -> bytes:
        reader, writer = await self.tunnel()
        writer.write(request)
        return await asyncio.wait_for(reader.read(), 5)

    async def head_of(self, request: bytes) -> str:
        await self.send(request)
        return (await asyncio.wait_for(self.received.get(), 5)).decode()

    async def test_the_bottles_https_request_reaches_the_server_with_the_credential(self) -> None:
        response = await self.send(
            b"GET /app/rest/server HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Bearer stand-in\r\n"
            b"Accept: */*\r\n\r\n"
        )
        head = (await asyncio.wait_for(self.received.get(), 5)).decode()
        self.assertTrue(response.endswith(b"hello"))
        self.assertTrue(head.startswith("GET /app/rest/server HTTP/1.1\r\n"))
        self.assertIn("Authorization: Bearer real-token", head)
        self.assertNotIn("stand-in", head)
        self.assertIn("Accept: */*", head)

    async def test_a_clienthello_smolvm_didnt_wait_for_still_gets_the_credential(self) -> None:
        for shown in (0, 10):  # none of it, and a part without the name
            with self.subTest(shown=shown):
                self.shown = shown
                head = await self.head_of(b"GET / HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Bearer stand-in\r\n\r\n")
                self.assertIn("Authorization: Bearer real-token", head)

    async def test_a_late_clienthello_for_another_host_goes_where_the_bottle_connected_untouched(self) -> None:
        self.shown, self.connected_to = 0, "127.0.0.1"
        # Not the egress CA's: the bottle's TLS goes through to the server, whose certificate it sees.
        anything = ssl.create_default_context()
        anything.check_hostname, anything.verify_mode = False, ssl.CERT_NONE
        reader, writer = await self.tunnel("elsewhere.test", trust=anything)
        writer.write(b"GET / HTTP/1.1\r\nHost: elsewhere.test\r\nAuthorization: Bearer stand-in\r\n\r\n")
        response = await asyncio.wait_for(reader.read(), 5)
        head = (await asyncio.wait_for(self.received.get(), 5)).decode()
        self.assertTrue(response.endswith(b"hello"))
        self.assertIn("Authorization: Bearer stand-in", head)
        self.assertNotIn("real-token", head)

    async def test_a_request_that_doesnt_ask_for_the_credential_goes_without_it(self) -> None:
        # Over TLS all the same: the upstream speaks nothing else.
        response = await self.send(b"GET /releases/download/x.zip HTTP/1.1\r\nHost: upstream.test\r\n\r\n")
        head = (await asyncio.wait_for(self.received.get(), 5)).decode()
        self.assertTrue(response.startswith(b"HTTP/1.1 200 "))
        self.assertNotIn("authorization", head.lower())

    async def test_a_token_of_the_bottles_own_goes_as_it_was_sent(self) -> None:
        for sent in (b"Bearer made-in-the-bottle", b"Basic " + base64.b64encode(b"me:my-password")):
            with self.subTest(sent):
                head = await self.head_of(b"GET / HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: " + sent + b"\r\n\r\n")
                self.assertIn("Authorization: " + sent.decode(), head)
                self.assertNotIn("real-token", head)

    async def test_basic_credentials_with_the_stand_in_get_the_real_one_as_their_password(self) -> None:
        # What git sends with what its credential helper gave it.
        basic = base64.b64encode(b"x-access-token:stand-in")
        head = await self.head_of(b"GET / HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Basic " + basic + b"\r\n\r\n")
        self.assertIn("Authorization: Basic " + base64.b64encode(b"x-access-token:real-token").decode(), head)
        self.assertEqual(head.lower().count("authorization:"), 1)

    async def test_the_header_is_found_whatever_its_case(self) -> None:
        head = await self.head_of(b"GET / HTTP/1.1\r\nHost: upstream.test\r\nauthorization: bearer stand-in\r\n\r\n")
        self.assertIn("Authorization: Bearer real-token", head)
        self.assertEqual(head.lower().count("authorization:"), 1)

    async def test_the_bottle_is_shown_a_certificate_for_the_host_it_asked_for(self) -> None:
        _, writer = await self.tunnel()
        certificate = writer.get_extra_info("peercert")
        self.assertIn(("DNS", "upstream.test"), certificate["subjectAltName"])

    async def test_a_client_that_doesnt_trust_the_ca_gets_no_request_through(self) -> None:
        with self.assertRaises(ssl.SSLCertVerificationError):
            await self.tunnel(trust=ssl.create_default_context())
        self.assertTrue(self.received.empty())

    async def test_the_log_says_a_credential_was_attached_and_what_the_server_answered(self) -> None:
        with self.assertLogs("bottle.egress") as logs:
            await self.head_of(b"GET /v1/models HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Bearer stand-in\r\n\r\n")
            await asyncio.sleep(0.05)
            await self.head_of(b"GET /v1/public HTTP/1.1\r\nHost: upstream.test\r\n\r\n")
            await asyncio.sleep(0.05)
        port = self.upstream_port
        self.assertRegex(logs.output[-2], rf"test {CONNECTED_TO}:{port} \(upstream.test\) \+credential -> 127.0.0.1 ok 200 ")
        self.assertRegex(logs.output[-1], rf"test {CONNECTED_TO}:{port} \(upstream.test\) -> 127.0.0.1 ok 200 ")
        self.assertNotIn("real-token", "\n".join(logs.output))

    async def test_with_the_credential_nothing_else_the_bottle_sent_under_that_name_leaves(self) -> None:
        head = await self.head_of(
            b"GET / HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Bearer stand-in\r\n"
            b"authorization: Bearer guessed-at-in-the-bottle\r\n\r\n"
        )
        self.assertNotIn("guessed-at-in-the-bottle", head)
        self.assertEqual(head.lower().count("authorization:"), 1)
        self.assertIn("Authorization: Bearer real-token", head)

    asking = (b"", b"Authorization: Bearer stand-in\r\n")  # with the credential and without

    async def test_the_host_header_goes_with_the_destination(self) -> None:
        for auth in self.asking:
            with self.subTest(auth):
                head = await self.head_of(b"GET / HTTP/1.1\r\nHost: elsewhere.test\r\n" + auth + b"\r\n")
                self.assertIn(f"Host: upstream.test:{self.upstream_port}", head)
                self.assertNotIn("elsewhere.test", head)

    async def test_headers_that_would_reroute_a_credentialed_request_never_leave(self) -> None:
        for auth in self.asking:
            with self.subTest(auth):
                head = await self.head_of(
                    b"GET / HTTP/1.1\r\nHost: upstream.test\r\nX-Forwarded-Host: elsewhere.test\r\n" + auth +
                    b"forwarded: host=elsewhere.test\r\nX-Original-URL: /admin\r\nX-HTTP-Method-Override: DELETE\r\n"
                    b"X-Real-IP: 10.0.0.1\r\nX-GitHub-Api-Version: 2022-11-28\r\n\r\n"
                )
                for gone in ("elsewhere.test", "/admin", "DELETE", "10.0.0.1"):
                    self.assertNotIn(gone, head)
                self.assertIn("X-GitHub-Api-Version: 2022-11-28", head)

    async def test_a_request_for_another_host_inside_the_tls_is_refused(self) -> None:
        response = await self.send(b"GET https://elsewhere.test/ HTTP/1.1\r\n\r\n")
        self.assertTrue(response.startswith(b"HTTP/1.1 400 "))
        self.assertTrue(self.received.empty())

    async def test_a_request_the_server_would_echo_back_is_refused(self) -> None:
        # TRACE answers with the request it received, and that would have the credential in it.
        for method in (b"TRACE", b"trace", b"TRACK"):
            with self.subTest(method):
                response = await self.send(method + b" / HTTP/1.1\r\nHost: upstream.test\r\n\r\n")
                self.assertTrue(response.startswith(b"HTTP/1.1 405 "))
                self.assertTrue(self.received.empty())

    async def test_a_credentials_host_may_be_private_but_only_for_an_injected_request(self) -> None:
        await self.head_of(b"GET / HTTP/1.1\r\n\r\n")
        self.assertEqual(self.asked[-1], ("upstream.test", True))
        # Another port is an ordinary flow, judged by the policy alone.
        await self.gateway.decide(Flow("127.0.0.1", 1, client_hello("upstream.test")))
        self.assertEqual(self.asked[-1], ("upstream.test", False))

    async def test_a_destination_the_bottle_may_not_reach_gets_no_certificate(self) -> None:
        minted = []

        def certificates(host: str) -> ssl.SSLContext:
            minted.append(host)
            return ca.server_context("test", ("upstream.test",), host)

        injections = tuple(egress.Injection(host=h, header="Authorization", value="Bearer real-token")
                           for h in ("metadata.test", "nowhere.test"))
        gateway = Gateway("test", LoopbackPolicy(), fake_resolve, injections=injections, certificates=certificates)
        for name, status in (("metadata.test", 403), ("nowhere.test", 502)):
            with self.subTest(name), self.assertRaises(Refused) as refused:
                await gateway.decide(Flow(CONNECTED_TO, 443, client_hello(name)))
            self.assertEqual(refused.exception.status, status)
        self.assertEqual(minted, [])

    async def test_plain_http_to_a_credentials_host_and_port_gets_no_credential(self) -> None:
        # A credential goes only on TLS: a Host header doesn't claim one.
        decision = await self.gateway.decide(
            Flow("127.0.0.1", self.upstream_port, b"GET / HTTP/1.1\r\nHost: upstream.test\r\n\r\n"))
        self.assertEqual(decision, Direct(("127.0.0.1",)))
        self.assertTrue(all(private is False for _, private in self.asked))


class InjectionMatchTest(unittest.TestCase):
    """Which flows bottled terminates to attach a credential."""

    injections = (
        egress.Injection(host="ci.test", header="Authorization", value="Bearer ci-token"),
        egress.Injection(host="*.mirror.test", header="X-Auth", value="mirror-token", port=8111),
    )

    def injection_for(self, target: str, certificates=lambda host: None) -> egress.Injection | None:
        gateway = Gateway("test", Policy(), fake_resolve, injections=self.injections, certificates=certificates)
        host, _, port = target.rpartition(":")
        return gateway._injection_at(host, int(port))

    def test_the_host_a_credential_claims(self) -> None:
        self.assertEqual(self.injection_for("ci.test:443"), self.injections[0])
        self.assertEqual(self.injection_for("CI.TEST:443"), self.injections[0])

    def test_a_pattern_and_its_port(self) -> None:
        self.assertEqual(self.injection_for("eu.mirror.test:8111"), self.injections[1])

    def test_somewhere_else_gets_nothing(self) -> None:
        for target in ("elsewhere.test:443", "ci.test.evil.example:443", "mirror.test:8111", "ci.test:8443",
                       "eu.mirror.test:443"):
            with self.subTest(target):
                self.assertIsNone(self.injection_for(target))

    def test_nothing_without_certificates(self) -> None:
        self.assertIsNone(self.injection_for("ci.test:443", certificates=None))


class ReplacingTest(unittest.TestCase):
    """Which header values present the stand-in, and what goes in their place."""

    bearer = egress.Injection(host="ci.test", header="Authorization", value="Bearer real",
                              standin="stand-in", credential="real")

    def test_the_stand_in_as_the_token_whatever_the_scheme(self) -> None:
        for presented in ("Bearer stand-in", " token stand-in ", "bearer stand-in"):
            with self.subTest(presented):
                self.assertEqual(self.bearer.replacing(presented), "Bearer real")

    def test_the_stand_in_as_a_whole_value(self) -> None:
        raw = egress.Injection(host="ci.test", header="X-Auth", value="real", standin="stand-in")
        self.assertEqual(raw.replacing("stand-in"), "real")

    def test_the_stand_in_as_a_basic_password(self) -> None:
        basic = "Basic " + base64.b64encode(b"x-access-token:stand-in").decode()
        self.assertEqual(self.bearer.replacing(basic), "Basic " + base64.b64encode(b"x-access-token:real").decode())

    def test_anything_else_is_left(self) -> None:
        for presented in ("", "Bearer", "Bearer other", "Bearer stand-in-and-more", "Bearer xstand-in",
                          "Basic " + base64.b64encode(b"stand-in:other").decode(),
                          "Basic " + base64.b64encode(b"x:stand-in2").decode(), "Basic not-base64!"):
            with self.subTest(presented):
                self.assertIsNone(self.bearer.replacing(presented))

    def test_no_basic_without_the_bare_credential(self) -> None:
        injection = egress.Injection(host="ci.test", header="Authorization", value="Bearer real", standin="stand-in")
        self.assertIsNone(injection.replacing("Basic " + base64.b64encode(b"u:stand-in").decode()))

    def test_no_stand_in_matches_nothing(self) -> None:
        injection = egress.Injection(host="ci.test", header="Authorization", value="Bearer real")
        for presented in ("", "Bearer ", "Basic " + base64.b64encode(b"u:").decode()):
            with self.subTest(presented):
                self.assertIsNone(injection.replacing(presented))


if __name__ == "__main__":
    unittest.main()
