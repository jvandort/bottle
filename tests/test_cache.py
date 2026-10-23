import asyncio
import functools
import os
import shutil
import ssl
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock
from urllib.parse import urlsplit

from bottle import ca, cache, egress
from bottle.cache import Cached, Store
from bottle.egress import Direct, Flow, Gateway, Redirect
from tests.support import client_hello
from tests.test_egress import CONNECTED_TO, LoopbackPolicy, Upstream, fake_resolve

FILE = b"a file's bytes " * 1000


class StoreTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.store = Store(self.root)

    def keep(self, host: str, path: str, data: bytes) -> bool:
        partial = self.store.partial(host, path)
        partial.write(data)
        return partial.commit()

    def test_a_file_is_kept_under_its_host_and_path(self) -> None:
        self.assertTrue(self.keep("repo.test", "/pkg/a/1.0/a-1.0.tgz", b"tgz"))
        self.assertEqual((self.root / "repo.test/pkg/a/1.0/a-1.0.tgz").read_bytes(), b"tgz")
        with self.store.open("REPO.test", "/pkg/a/1.0/a-1.0.tgz") as kept:
            self.assertEqual(kept.read(), b"tgz")
        self.assertEqual(list((self.root / cache.PARTIAL).iterdir()), [])

    def test_nothing_kept_is_nothing_opened(self) -> None:
        self.assertIsNone(self.store.open("repo.test", "/pkg/a/1.0/a-1.0.tgz"))
        self.keep("repo.test", "/pkg/a/1.0/a-1.0.tgz", b"tgz")
        for path in ("/pkg/a/1.0", "/pkg/a/1.0/a-1.0.tgz/x"):  # a directory, and under a file
            with self.subTest(path):
                self.assertIsNone(self.store.open("repo.test", path))

    def test_paths_the_filesystem_would_read_as_others_have_no_file(self) -> None:
        for path in ("/", "/pkg/", "/pkg//a.tgz", "/pkg/../a.tgz", "/./a.tgz", "/..", "relative.tgz", "/a\0.tgz",
                     "/" + "x" * 256):
            with self.subTest(path):
                self.assertIsNone(self.store.file("repo.test", path))
                self.assertIsNone(self.store.partial("repo.test", path))
        self.assertIsNone(self.store.file("../etc", "/passwd"))
        self.assertIsNone(self.store.file("*.repo.test", "/a.tgz"))
        self.assertEqual(self.store.file("repo.test", "/a%2F..%2Fb.tgz"), self.root / "repo.test" / "a%2F..%2Fb.tgz")

    def test_a_file_where_a_directory_is_isnt_kept(self) -> None:
        self.keep("repo.test", "/pkg/a/1.0/a-1.0.tgz", b"tgz")
        self.assertFalse(self.keep("repo.test", "/pkg/a/1.0", b"listing"))
        self.assertFalse(self.keep("repo.test", "/pkg/a/1.0/a-1.0.tgz/x", b"under a file"))
        self.assertEqual((self.root / "repo.test/pkg/a/1.0/a-1.0.tgz").read_bytes(), b"tgz")
        self.assertEqual(list((self.root / cache.PARTIAL).iterdir()), [])

    def test_a_discarded_file_leaves_nothing(self) -> None:
        partial = self.store.partial("repo.test", "/a.tgz")
        partial.write(b"half")
        partial.discard()
        self.assertIsNone(self.store.open("repo.test", "/a.tgz"))
        self.assertEqual(list((self.root / cache.PARTIAL).iterdir()), [])

    def test_a_name_on_disk_in_another_case_is_another_file(self) -> None:
        # What a Mac's filesystem says when it opens foo.tgz for Foo.tgz.
        self.keep("repo.test", "/pkg/foo.tgz", b"foo")
        named = lambda fd, file: str(file).endswith("foo.tgz") or str(file).endswith("org")  # noqa: E731
        with mock.patch.object(Store, "_named", lambda self, fd, file: named(fd, file)):
            self.assertIsNone(self.store.open("repo.test", "/pkg/Foo.tgz"))
            with self.store.open("repo.test", "/pkg/foo.tgz") as kept:
                self.assertEqual(kept.read(), b"foo")
        with mock.patch.object(Store, "_named", lambda self, fd, file: False):
            self.assertFalse(self.keep("repo.test", "/Pkg/bar.tgz", b"bar"))

    def test_on_a_filesystem_that_ignores_case_a_name_in_another_case_is_another_file(self) -> None:
        # A Mac's, by default: run there, this is the real thing, not a mock.
        self.keep("repo.test", "/pkg/foo/foo.tgz", b"foo")
        if not (self.root / "repo.test/pkg/FOO/Foo.tgz").exists():
            self.skipTest("this filesystem tells names apart by case")
        self.assertIsNone(self.store.open("repo.test", "/pkg/FOO/Foo.tgz"))
        self.assertIsNone(self.store.open("repo.test", "/pkg/foo/Foo.tgz"))
        with self.store.open("repo.test", "/pkg/foo/foo.tgz") as kept:
            self.assertEqual(kept.read(), b"foo")
        # Kept under FOO, it would be found under foo.
        self.assertFalse(self.keep("repo.test", "/pkg/FOO/bar.tgz", b"bar"))
        self.assertFalse((self.root / "repo.test/pkg/foo/bar.tgz").exists())
        # The same name in another case replaces the file, which is then found under its new name only.
        self.assertTrue(self.keep("repo.test", "/pkg/foo/FOO.tgz", b"FOO"))
        self.assertIsNone(self.store.open("repo.test", "/pkg/foo/foo.tgz"))
        with self.store.open("repo.test", "/pkg/foo/FOO.tgz") as kept:
            self.assertEqual(kept.read(), b"FOO")

    def aged(self, file: Path, seconds: float) -> None:
        then = __import__("time").time() - seconds
        os.utime(file, (then, then))

    def test_a_hit_is_a_use(self) -> None:
        self.keep("repo.test", "/a.tgz", b"a")
        self.aged(self.root / "repo.test/a.tgz", cache.EXPIRY - 60)
        self.store.open("repo.test", "/a.tgz").close()
        self.store.expire()
        self.assertTrue((self.root / "repo.test/a.tgz").exists())

    def test_files_unused_for_a_week_are_removed_with_the_directories_they_leave_empty(self) -> None:
        for path in ("/pkg/old/1.0/old.tgz", "/pkg/new/1.0/new.tgz"):
            self.keep("repo.test", path, b"tgz")
        self.aged(self.root / "repo.test/pkg/old/1.0/old.tgz", cache.EXPIRY + 60)
        self.aged(self.root / "repo.test/pkg/new/1.0/new.tgz", cache.EXPIRY - 60)
        self.store.expire()
        self.assertFalse((self.root / "repo.test/pkg/old").exists())
        self.assertTrue((self.root / "repo.test/pkg/new/1.0/new.tgz").exists())
        self.assertTrue((self.root / cache.PARTIAL).is_dir())
        Store(self.root / "nothing-yet").expire()

    def test_partial_files_left_behind_are_removed_after_a_day(self) -> None:
        old, new = self.store.partial("repo.test", "/old.tgz"), self.store.partial("repo.test", "/new.tgz")
        self.addCleanup(old.out.close)
        self.addCleanup(new.out.close)
        self.aged(old.temporary, cache.PARTIAL_EXPIRY + 60)
        self.store.expire()
        self.assertFalse(old.temporary.exists())
        self.assertTrue(new.temporary.exists())


class CachedTest(unittest.TestCase):
    def test_matches_and_excludes(self) -> None:
        cached = Cached("*.mirror.test", ("*/index.json*", "/latest/*"))
        self.assertTrue(cached.matches("Repo.Mirror.test"))
        self.assertFalse(cached.matches("mirror.test"))
        self.assertTrue(cached.excludes("/pkg/a/index.json"))
        self.assertTrue(cached.excludes("/pkg/a/index.json.sha256"))
        self.assertTrue(cached.excludes("/latest/a.tgz"))
        self.assertFalse(cached.excludes("/pkg/a/1.0/a-1.0.tgz"))



@unittest.skipUnless(shutil.which("openssl"), "needs openssl to make a certificate")
class CachingTest(unittest.IsolatedAsyncioTestCase):
    """A bottle's downloads from a cached host, as a bottle makes them: HTTPS, kept alive."""

    async def asyncSetUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        certificate, key = self.tmp / "cert.pem", self.tmp / "key.pem"
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-keyout", str(key), "-out", str(certificate), "-subj", "/CN=upstream.test",
             "-addext", "subjectAltName=DNS:upstream.test,DNS:elsewhere.test"],
            check=True, capture_output=True,
        )
        serving = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        serving.load_cert_chain(certificate, key)
        self.received: list[str] = []
        upstream = await asyncio.start_server(self.upstream, "127.0.0.1", 0, ssl=serving)
        self.addAsyncCleanup(Upstream.close, upstream)
        self.upstream_port = upstream.sockets[0].getsockname()[1]
        # The test's upstream is a cached host's https port.
        for patcher in (
            mock.patch.object(egress, "CACHE_PORT", self.upstream_port),
            mock.patch.object(egress, "_tls_context", lambda: ssl.create_default_context(cafile=str(certificate))),
            mock.patch.dict("os.environ", {"BOTTLE_HOME": str(self.tmp / "home")}),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.bottle_trusts = ssl.create_default_context(cadata=ca.certificate("test", ("upstream.test",)))
        self.store = Store(self.tmp / "cache")
        self.gateway = Gateway(
            "test", LoopbackPolicy(), fake_resolve,
            certificates=functools.partial(ca.server_context, "test", ("upstream.test",)),
            caches=(Cached("upstream.test", ("*/index.json",)),), store=self.store,
        )
        server = await asyncio.start_server(self.decider, "127.0.0.1", 0)
        self.addAsyncCleanup(Upstream.close, server)
        self.port = server.sockets[0].getsockname()[1]

    async def decider(self, reader, writer) -> None:
        first = await reader.read(64 * 1024)
        flow = Flow(CONNECTED_TO, self.upstream_port, first)
        await self.gateway.serve(flow, await self.gateway.decide(flow), reader, writer)

    async def upstream(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """Answers one request, by its path, after recording its head."""
        head = (await reader.readuntil(b"\r\n\r\n")).decode()
        self.received.append(head)
        method, target, _ = head.split("\r\n")[0].split(" ")
        path, here = urlsplit(target).path, f"https://upstream.test:{self.upstream_port}"
        if path.endswith(".tgz") or path.endswith("index.json"):
            response = (b"HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nSet-Cookie: cdn=1; Domain=elsewhere.test\r\n"
                        b"Content-Length: %d\r\n\r\n" % len(FILE))
            response += FILE if method == "GET" else b""
        elif path == "/chunked.json":
            response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n<ok/\r\n1;x=y\r\n>\r\n0\r\nX-Trailer: t\r\n\r\n"
        elif path == "/no-store-first.json":
            response = b"HTTP/1.1 200 OK\r\nCache-Control: no-store\r\nCache-Control: max-age=60\r\nContent-Length: 2\r\n\r\nok"
        elif path == "/short.zip":
            response = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nnot 100 bytes"
        elif path.startswith("/uncacheable/"):
            response = f"HTTP/1.1 200 OK\r\nCache-Control: {path.split('/')[2]}\r\nContent-Length: 2\r\n\r\nok".encode()
        elif path == "/gzipped.json":
            response = b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 4\r\n\r\ngzip"
        elif path == "/redirected.zip":
            response = f"HTTP/1.1 307 Temporary Redirect\r\nLocation: {here}/elsewhere/a.tgz?signed=1\r\nContent-Length: 0\r\n\r\n".encode()
        elif path == "/to-plain-http.zip":
            response = b"HTTP/1.1 302 Found\r\nLocation: http://upstream.test/a.tgz\r\nContent-Length: 0\r\n\r\n"
        elif path == "/to-the-host.zip":
            response = b"HTTP/1.1 302 Found\r\nLocation: https://metadata.test/a.tgz\r\nContent-Length: 0\r\n\r\n"
        else:
            response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found"
        writer.write(response)
        await writer.drain()
        writer.close()

    async def connect(self) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        reader, writer = await asyncio.wait_for(asyncio.open_connection(
            "127.0.0.1", self.port, ssl=self.bottle_trusts, server_hostname="upstream.test"), 5)
        self.addCleanup(writer.close)
        return reader, writer

    async def exchange(self, reader, writer, method: str, path: str, *headers: str) -> tuple[int, dict[str, str], bytes]:
        """One request on a kept-alive connection: the response's status, headers and body."""
        writer.write("\r\n".join([f"{method} {path} HTTP/1.1", "Host: upstream.test", *headers, "", ""]).encode())
        response = await asyncio.wait_for(egress._read_response(reader), 5)
        body = b"".join([data async for data in egress._body(reader, egress._body_length(method, response))])
        return response.status, response.headers, body

    async def get(self, path: str, *headers: str, method: str = "GET") -> tuple[int, dict[str, str], bytes]:
        return await self.exchange(*await self.connect(), method, path, *headers)

    def kept(self, path: str) -> Path:
        return self.tmp / "cache" / "upstream.test" / path.lstrip("/")

    async def stored(self, path: str) -> bytes:
        """What was kept for `path`: it's put in place just after the bottle has the last of it."""
        for _ in range(100):
            if self.kept(path).exists():
                return self.kept(path).read_bytes()
            await asyncio.sleep(0.01)
        self.fail(f"{path} wasn't kept")

    async def test_a_download_is_kept_and_the_next_one_answered_from_it(self) -> None:
        first = await self.get("/pkg/a/1.0/a-1.0.tgz")
        self.assertEqual(first[:1] + first[2:], (200, FILE))
        self.assertEqual(await self.stored("/pkg/a/1.0/a-1.0.tgz"), FILE)
        second = await self.get("/pkg/a/1.0/a-1.0.tgz")
        self.assertEqual((second[0], second[2], second[1]["content-length"]), (200, FILE, str(len(FILE))))
        self.assertNotIn("content-type", second[1])  # none was kept
        self.assertEqual(len(self.received), 1)

    async def test_a_head_is_answered_from_what_was_kept_and_passed_on_when_nothing_was(self) -> None:
        status, headers, body = await self.get("/pkg/a/1.0/a-1.0-sources.tgz", method="HEAD")
        self.assertEqual((status, headers["content-length"], body), (200, str(len(FILE)), b""))
        self.assertTrue(self.received[-1].startswith("HEAD /pkg/a/1.0/a-1.0-sources.tgz "))
        self.assertFalse(self.kept("/pkg/a/1.0/a-1.0-sources.tgz").exists())
        await self.get("/pkg/a/1.0/a-1.0-sources.tgz")
        await self.stored("/pkg/a/1.0/a-1.0-sources.tgz")
        status, headers, body = await self.get("/pkg/a/1.0/a-1.0-sources.tgz", method="HEAD")
        self.assertEqual((status, headers["content-length"], body), (200, str(len(FILE)), b""))
        self.assertEqual(len(self.received), 2)
        self.assertEqual((await self.get("/missing.tgz.json", method="HEAD"))[0], 404)

    async def test_requests_on_one_connection_are_answered_in_turn(self) -> None:
        reader, writer = await self.connect()
        for path in ("/a.tgz", "/a.tgz", "/missing", "/chunked.json", "/b.tgz"):
            with self.subTest(path):
                status, _, body = await self.exchange(reader, writer, "GET", path)
                self.assertEqual(status, 404 if path == "/missing" else 200)
        self.assertEqual([h.split(" ")[1] for h in self.received], ["/a.tgz", "/missing", "/chunked.json", "/b.tgz"])

    async def test_the_upstream_gets_bottleds_request_not_the_bottles(self) -> None:
        await self.get("/a.tgz", "Accept-Encoding: gzip", "Accept: text/html", "X-Anything: 1", "User-Agent: some-tool/1.0")
        head = self.received[-1]
        self.assertIn("Accept-Encoding: identity", head)
        self.assertIn("User-Agent: some-tool/1.0", head)
        self.assertNotIn("X-Anything", head)
        self.assertNotIn("text/html", head)

    async def test_a_redirect_is_followed_and_kept_as_if_it_wasnt_there(self) -> None:
        status, headers, body = await self.get("/redirected.zip")
        self.assertEqual((status, body), (200, FILE))
        self.assertNotIn("location", headers)
        self.assertTrue(self.received[-1].startswith("GET /elsewhere/a.tgz?signed=1 "))
        self.assertEqual(await self.stored("/redirected.zip"), FILE)
        self.assertFalse(self.kept("/elsewhere/a.tgz").exists())

    async def test_a_cached_host_sets_the_bottle_no_cookies(self) -> None:
        # One sent back would make every request after it pass the store by.
        for request in (("/a.tgz",), ("/redirected.zip",), ("/a.tgz?v=1",), ("/a.tgz", "Authorization: Bearer x")):
            with self.subTest(request):
                self.assertNotIn("set-cookie", (await self.get(*request))[1])
        self.assertNotIn("set-cookie", (await self.get("/a.tgz", method="HEAD"))[1])

    async def test_a_redirect_bottled_wont_follow_reaches_the_bottle(self) -> None:
        for path in ("/to-plain-http.zip", "/to-the-host.zip"):
            with self.subTest(path):
                status, headers, _ = await self.get(path)
                self.assertEqual(status, 302)
                self.assertIn("location", headers)
                self.assertFalse(self.kept(path).exists())

    async def test_only_a_whole_200_of_the_file_as_it_is_is_kept(self) -> None:
        self.assertEqual((await self.get("/missing.tgz.json"))[0], 404)
        self.assertEqual((await self.get("/gzipped.json"))[0], 200)
        with self.assertRaises(egress.Refused):  # the bottle sees it end early, as bottled did
            await self.get("/short.zip")
        await asyncio.sleep(0.05)
        for path in ("/missing.tgz.json", "/gzipped.json", "/short.zip"):
            self.assertFalse(self.kept(path).exists(), path)
        self.assertEqual(list((self.tmp / "cache" / cache.PARTIAL).iterdir()), [])

    async def test_what_the_server_says_not_to_keep_or_to_check_first_isnt_kept(self) -> None:
        await self.get("/no-store-first.json")  # on the first of several Cache-Control lines
        await asyncio.sleep(0.05)
        self.assertFalse(self.kept("/no-store-first.json").exists())
        for directive in ("no-store", "no-cache", "private", "max-age=0"):
            with self.subTest(directive):
                path = f"/uncacheable/{directive}/a.json"
                self.assertEqual((await self.get(path))[2], b"ok")
                await asyncio.sleep(0.05)
                self.assertFalse(self.kept(path).exists())

    async def test_a_chunked_download_is_kept_whole(self) -> None:
        self.assertEqual((await self.get("/chunked.json"))[2], b"<ok/>")
        self.assertEqual(await self.stored("/chunked.json"), b"<ok/>")

    async def test_what_could_be_someone_elses_answer_is_never_kept_or_answered_from_the_store(self) -> None:
        await self.get("/a.tgz")
        await self.stored("/a.tgz")
        for request in (("/a.tgz", "Authorization: Bearer x"), ("/a.tgz", "Cookie: session=1"), ("/a.tgz", "Range: bytes=0-1"),
                        ("/a.tgz?v=2",), ("/b.tgz", "Authorization: Bearer x"), ("/meta/index.json",)):
            with self.subTest(request):
                before = len(self.received)
                await self.get(*request)
                self.assertEqual(len(self.received), before + 1)
        self.assertIn("Authorization: Bearer x", self.received[-2])
        self.assertFalse(self.kept("/b.tgz").exists())
        self.assertFalse(self.kept("/meta/index.json").exists())

    async def test_no_cache_fetches_again_and_keeps_what_it_got(self) -> None:
        await self.get("/a.tgz")
        await self.stored("/a.tgz")
        await self.get("/a.tgz", "Cache-Control: no-cache")
        self.assertEqual(len(self.received), 2)

    async def test_anything_but_get_and_head_passes_through_and_ends_the_connection(self) -> None:
        reader, writer = await self.connect()
        writer.write(b"POST /upload.tgz HTTP/1.1\r\nHost: upstream.test\r\nContent-Length: 2\r\n\r\nhi")
        response = await asyncio.wait_for(reader.read(), 5)
        self.assertTrue(response.startswith(b"HTTP/1.1 200 "))
        self.assertTrue(self.received[-1].startswith("POST /upload.tgz "))
        self.assertFalse(self.kept("/upload.tgz").exists())

    async def test_each_request_is_logged_with_what_became_of_it(self) -> None:
        with self.assertLogs("bottle.egress") as logs:
            await self.get("/a.tgz")
            await self.stored("/a.tgz")
            await self.get("/a.tgz")
            await self.get("/a.tgz?v=1")
            await self.get("/missing.json")
            await self.get("/b.tgz", method="HEAD")
            await asyncio.sleep(0.05)
        requests = [line for line in logs.output if " GET https://" in line]
        self.assertRegex(requests[0], r"test GET https://upstream.test/a.tgz stored 200$")
        self.assertRegex(requests[1], r"test GET https://upstream.test/a.tgz hit 200$")
        self.assertRegex(requests[2], r"test GET https://upstream.test/a.tgz\?v=1 passed 200$")
        self.assertRegex(requests[3], r"test GET https://upstream.test/missing.json fetched 404$")
        heads = [line for line in logs.output if " HEAD https://" in line]
        self.assertRegex(heads[0], r"test HEAD https://upstream.test/b.tgz passed 200$")


class DecideTest(unittest.IsolatedAsyncioTestCase):
    def gateway(self, **kwargs) -> Gateway:
        return Gateway("test", LoopbackPolicy(), fake_resolve, certificates=lambda host: None,
                       caches=(Cached("upstream.test"),), store=Store(Path("/nonexistent")), **kwargs)

    async def test_tls_to_a_cached_host_is_bottleds(self) -> None:
        decision = await self.gateway().decide(Flow(CONNECTED_TO, 443, client_hello("upstream.test")))
        self.assertEqual(decision, Redirect(egress.CACHE, addresses=("127.0.0.1",), cached=Cached("upstream.test")))

    async def test_anything_else_goes_direct(self) -> None:
        for flow in (Flow("127.0.0.1", 8443, client_hello("upstream.test")), Flow("127.0.0.1", 443, client_hello("elsewhere.test")),
                     Flow("127.0.0.1", 80, b"GET / HTTP/1.1\r\nHost: upstream.test\r\n\r\n")):
            with self.subTest(flow):
                self.assertEqual(await self.gateway().decide(flow), Direct(("127.0.0.1",)))

    async def test_nothing_is_cached_without_certificates_or_a_store(self) -> None:
        for gateway in (Gateway("test", LoopbackPolicy(), fake_resolve, caches=(Cached("upstream.test"),), store=Store()),
                        Gateway("test", LoopbackPolicy(), fake_resolve, certificates=lambda h: None, caches=(Cached("upstream.test"),))):
            decision = await gateway.decide(Flow("127.0.0.1", 443, client_hello("upstream.test")))
            self.assertEqual(decision, Direct(("127.0.0.1",)))

    async def test_a_credential_claims_its_host_before_a_cache_does(self) -> None:
        injection = egress.Injection(host="upstream.test", header="Authorization", value="Bearer x")
        decision = await self.gateway(injections=(injection,)).decide(Flow(CONNECTED_TO, 443, client_hello("upstream.test")))
        self.assertEqual(decision.service, egress.CREDENTIAL)

    async def test_a_late_clienthello_on_the_cached_port_is_read_by_bottled(self) -> None:
        decision = await self.gateway().decide(Flow(CONNECTED_TO, 443, b""))
        self.assertEqual(decision.service, egress.OPENING)
