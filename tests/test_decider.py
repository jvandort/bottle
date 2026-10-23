import asyncio
import ipaddress
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import daemon, decider, host
from bottle.daemon import Daemon
from bottle.decider import ALLOW_DIRECT, DENY, REDIRECT, BadPrelude, Decider, read_prelude

MACHINE = bytes(range(1, 17))


def prelude(token: bytes, address: str, port: int, first: bytes = b"", machine: bytes = MACHINE,
            magic: bytes = decider.MAGIC, length: int | None = None) -> bytes:
    """What smolvm writes for one guest flow."""
    ip = ipaddress.ip_address(address)
    return (
        magic + token + machine + bytes(16) + bytes([ip.version]) + port.to_bytes(2, "big") + ip.packed
        + (len(first) if length is None else length).to_bytes(2, "big") + first
    )


class PreludeTest(unittest.IsolatedAsyncioTestCase):
    async def read(self, data: bytes):
        reader = asyncio.StreamReader()
        reader.feed_data(data)
        reader.feed_eof()
        return await read_prelude(reader)

    async def test_an_ipv4_flow(self) -> None:
        token = os.urandom(32)
        read = await self.read(prelude(token, "93.184.216.34", 443, b"\x16\x03\x01"))
        self.assertEqual((read.token, read.machine_id, read.address, read.port, read.first),
                         (token, MACHINE, "93.184.216.34", 443, b"\x16\x03\x01"))

    async def test_an_ipv6_flow(self) -> None:
        self.assertEqual((await self.read(prelude(bytes(32), "2606:4700::1", 80))).address, "2606:4700::1")

    async def test_refused(self) -> None:
        for what, data in (
            ("another version", prelude(bytes(32), "93.184.216.34", 443, magic=b"SMOLMEG1")),
            ("no machine identity", prelude(bytes(32), "93.184.216.34", 443, machine=bytes(16))),
            ("more first bytes than allowed", prelude(bytes(32), "93.184.216.34", 443, length=8193)),
        ):
            with self.subTest(what), self.assertRaises(BadPrelude):
                await self.read(data)

    async def test_cut_short(self) -> None:
        whole = prelude(bytes(32), "93.184.216.34", 443, b"hello")
        for end in (0, 10, 75, 80, len(whole) - 1):
            with self.subTest(end), self.assertRaises(asyncio.IncompleteReadError):
                await self.read(whole[:end])


class DeciderTest(unittest.IsolatedAsyncioTestCase):
    """bottled's decider, as smolvm meets it: a token per bottle, and one byte per flow."""

    async def asyncSetUp(self) -> None:
        tmp = tempfile.mkdtemp(dir="/tmp")
        self.addCleanup(shutil.rmtree, tmp, True)
        self.tmp = Path(tmp)
        self.path = self.tmp / "d.sock"
        for patcher in (
            mock.patch.object(daemon.ca, "hosts", return_value=[]),
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon, "bound_bottles", side_effect=lambda: self.bound),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.bound = []
        await self.start()

    async def start(self) -> None:
        self.daemon = Daemon(decider=Decider(self.tmp / "tokens.json"), decider_port=0)
        self.task = asyncio.create_task(daemon.serve(self.path, self.daemon))
        for _ in range(200):
            if self.path.exists() and self.daemon.decider_address:
                break
            await asyncio.sleep(0.01)

    async def stop(self) -> None:
        await self.request({"op": "shutdown"})
        await asyncio.wait_for(self.task, 5)

    async def asyncTearDown(self) -> None:
        if not self.task.done():
            await self.stop()

    async def request(self, message: dict) -> dict:
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        writer.write(json.dumps(message).encode() + b"\n")
        reply = json.loads(await reader.readline())
        writer.close()
        return reply

    async def bind(self, bottle: str = "b") -> bytes:
        reply = await self.request({"op": "bind", "bottle": bottle})
        self.assertTrue(reply["ok"], reply)
        self.assertEqual(reply["decider"], self.daemon.decider_address)
        return bytes.fromhex(reply["token"])

    async def flow(self, data: bytes) -> tuple[bytes, asyncio.StreamReader, asyncio.StreamWriter]:
        address, port = self.daemon.decider_address.rsplit(":", 1)
        reader, writer = await asyncio.open_connection(address, int(port))
        self.addCleanup(writer.close)
        writer.write(data)
        return await asyncio.wait_for(reader.read(1), 5), reader, writer

    async def test_a_public_destination_is_allowed_direct(self) -> None:
        token = await self.bind()
        self.assertEqual((await self.flow(prelude(token, "93.184.216.34", 443)))[0], ALLOW_DIRECT)

    async def test_a_private_destination_is_denied(self) -> None:
        token = await self.bind()
        self.assertEqual((await self.flow(prelude(token, "192.168.1.1", 80)))[0], DENY)

    async def test_bottle_host_is_redirected_and_answered(self) -> None:
        token = await self.bind()
        head = b"GET /git/nothing/info/refs HTTP/1.1\r\nHost: bottle.host\r\n\r\n"
        answer, reader, _ = await self.flow(prelude(token, host.ADDRESS, 80, head))
        self.assertEqual(answer, REDIRECT)
        # No repo, so no remotes: bottle.host itself answers that there's nothing there.
        self.assertTrue((await asyncio.wait_for(reader.read(), 5)).startswith(b"HTTP/1.1 404"))

    async def test_a_token_no_bottle_has_gets_no_answer(self) -> None:
        await self.bind()
        self.assertEqual((await self.flow(prelude(os.urandom(32), "93.184.216.34", 443)))[0], b"")

    async def test_a_bad_prelude_gets_no_answer(self) -> None:
        token = await self.bind()
        self.assertEqual((await self.flow(prelude(token, "93.184.216.34", 443, magic=b"SMOLMEG9")))[0], b"")

    async def test_another_machine_with_the_token_is_denied(self) -> None:
        token = await self.bind()
        await self.flow(prelude(token, "93.184.216.34", 443))
        self.assertEqual((await self.flow(prelude(token, "93.184.216.34", 443, machine=bytes(range(2, 18)))))[0], DENY)
        self.assertEqual((await self.flow(prelude(token, "93.184.216.34", 443)))[0], ALLOW_DIRECT)

    async def test_binding_again_gives_a_new_token_and_retires_the_old(self) -> None:
        old = await self.bind()
        new = await self.bind()
        self.assertNotEqual(old, new)
        self.assertEqual((await self.flow(prelude(old, "93.184.216.34", 443)))[0], b"")
        self.assertEqual((await self.flow(prelude(new, "93.184.216.34", 443)))[0], ALLOW_DIRECT)

    async def test_release_forgets_the_token(self) -> None:
        token = await self.bind()
        await self.request({"op": "release", "bottle": "b"})
        self.assertEqual((await self.flow(prelude(token, "93.184.216.34", 443)))[0], b"")
        self.assertEqual(json.loads((self.tmp / "tokens.json").read_text()), {})

    async def test_tokens_are_owner_only(self) -> None:
        await self.bind()
        self.assertEqual((self.tmp / "tokens.json").stat().st_mode & 0o777, 0o600)

    async def test_a_restarted_bottled_honours_the_tokens_it_gave(self) -> None:
        token = await self.bind()
        await self.stop()
        self.bound = [("b", None, (), ())]
        await self.start()
        for _ in range(200):
            if self.daemon.decider.bindings:
                break
            await asyncio.sleep(0.01)
        self.assertEqual((await self.flow(prelude(token, "93.184.216.34", 443)))[0], ALLOW_DIRECT)

    async def test_a_restart_doesnt_restore_a_bottle_without_a_token(self) -> None:
        await self.stop()
        self.bound = [("other", None, (), ())]
        await self.start()
        await asyncio.sleep(0.05)
        self.assertEqual(self.daemon.decider.bindings, {})
