import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from bottle import daemon, messages
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
            # The hosts the bottle's CA was made for when it started.
            mock.patch.object(daemon.ca, "hosts", return_value=["ci.test"]),
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon.cache, "cached_for", return_value=[]),
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
        injections = [daemon.egress.Injection("ci.test", "Authorization", "Bearer the-real-credential")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=injections) as looked_up:
            reply = await self.request({"op": "bind", "bottle": "b", "features": ["teamcity:server=ci.test"]})
        looked_up.assert_called_once_with(("teamcity:server=ci.test",))
        self.assertNotIn("the-real-credential", json.dumps(reply))
        [binding] = self.daemon.decider.bindings.values()
        self.assertEqual(binding.gateway.injections, tuple(injections))

    async def test_a_bottles_gateway_mints_from_that_bottles_ca(self) -> None:
        with mock.patch.object(daemon.ca, "server_context") as server_context:
            await self.request({"op": "bind", "bottle": "b", "features": ["teamcity:server=ci.test"]})
            [binding] = self.daemon.decider.bindings.values()
            binding.gateway.certificates("ci.test")
        server_context.assert_called_once_with("b", ("ci.test",), "ci.test")

    async def test_only_what_the_bottles_ca_covers_is_terminated(self) -> None:
        # Named by its features since the bottle started, whose CA doesn't cover them: reached directly.
        injections = [daemon.egress.Injection(h, "Authorization", "Bearer x") for h in ("ci.test", "new-ci.test")]
        caches = [daemon.cache.Cached(h) for h in ("ci.test", "new-files.test")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=injections), \
                mock.patch.object(daemon.cache, "cached_for", return_value=caches):
            await self.request({"op": "bind", "bottle": "b", "features": ["f"]})
        [binding] = self.daemon.decider.bindings.values()
        self.assertEqual([i.host for i in binding.gateway.injections], ["ci.test"])
        self.assertEqual([c.host for c in binding.gateway.caches], ["ci.test"])

    async def test_binding_again_reads_the_credentials_again(self) -> None:
        for value in ("Bearer old", "Bearer new"):
            injections = [daemon.egress.Injection("ci.test", "Authorization", value)]
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


class DeliverTest(unittest.IsolatedAsyncioTestCase):
    """Daemon.deliver and Daemon.peers: messages between bottles, run in the recipient by way of a fake smolvm."""

    async def asyncSetUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        # Records its arguments and stdin, and exits as $FAKE_EXIT says.
        smolvm = self.tmp / "smolvm"
        smolvm.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$FAKE_DIR/argv"\ncat > "$FAKE_DIR/stdin"\n'
                          'echo "$FAKE_ERR" >&2\nexit "${FAKE_EXIT:-0}"\n')
        smolvm.chmod(0o755)
        env = {"BOTTLE_SMOLVM": str(smolvm), "FAKE_DIR": str(self.tmp), "FAKE_ERR": "", "BOTTLE_HOME": str(self.tmp)}
        patcher = mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)
        for patcher in (
            mock.patch.object(daemon.ca, "hosts", return_value=[]),
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon.cache, "cached_for", return_value=[]),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.daemon = Daemon(decider=Decider(self.tmp / "tokens.json"))

        def bottle(name: str, repo: str | None = None, **spec) -> mock.Mock:
            return SimpleNamespace(machine=f"bottle-{name}", git_repo=repo, spec=spec)

        registry = {
            "broker": bottle("broker", "/Users/me/memories", permissions=["list-bottles", "message-any"]),
            "app-1a2b3c4d-foo": bottle("app-1a2b3c4d-foo", "/Users/me/src/app", thread="foo"),
            "app-1a2b3c4d-bar": bottle("app-1a2b3c4d-bar", "/Users/me/src/app", thread="bar"),
            "stopped": bottle("stopped"),
        }
        loaded = mock.patch("bottle.bottles.load", return_value=registry)
        loaded.start()
        self.addCleanup(loaded.stop)
        for name in ("broker", "app-1a2b3c4d-foo", "app-1a2b3c4d-bar"):
            await self.daemon.bind(name)

    def ran(self) -> tuple[list[str], str]:
        """What the fake smolvm ran last, after `--`, and in which machine; and its stdin."""
        argv = (self.tmp / "argv").read_text().splitlines()
        return argv[argv.index("--name") + 1], argv[argv.index("--") + 1:], (self.tmp / "stdin").read_text()

    async def refused(self, sender: str, to: str) -> daemon.ServiceError:
        with self.assertRaises(daemon.ServiceError) as raised:
            await self.daemon.deliver(sender, to, "hi")
        return raised.exception

    async def test_a_bottle_with_message_any_sends_by_name_from_an_address_issued_to_the_recipient(self) -> None:
        await self.daemon.deliver("broker", "app-1a2b3c4d-foo", "status?\nall of it")
        machine, argv, stdin = self.ran()
        self.assertEqual("bottle-app-1a2b3c4d-foo", machine)
        reply_to = messages.address("broker", holder="app-1a2b3c4d-foo")
        self.assertEqual([daemon.BOTTLE_EVENT, "send", "-t", str(daemon.DELIVERY_TIMEOUT), "-f", reply_to], argv)
        self.assertEqual("status?\nall of it", stdin)

    async def test_the_recipient_answers_by_the_address_it_was_given(self) -> None:
        await self.daemon.deliver("app-1a2b3c4d-foo", messages.address("broker", holder="app-1a2b3c4d-foo"), "done")
        machine, argv, _ = self.ran()
        self.assertEqual("bottle-broker", machine)
        self.assertEqual(messages.address("app-1a2b3c4d-foo", holder="broker"), argv[-1])

    async def test_a_bottle_without_message_any_cant_send_by_name(self) -> None:
        self.assertEqual(404, (await self.refused("app-1a2b3c4d-foo", "broker")).status)
        self.assertFalse((self.tmp / "argv").exists())

    async def test_an_address_works_only_for_the_bottle_it_was_issued_to(self) -> None:
        issued_to_foo = messages.address("broker", holder="app-1a2b3c4d-foo")
        self.assertEqual(404, (await self.refused("app-1a2b3c4d-bar", issued_to_foo)).status)
        self.assertFalse((self.tmp / "argv").exists())

    async def test_a_forged_address_is_refused_like_a_missing_bottle(self) -> None:
        forged = await self.refused("app-1a2b3c4d-foo", "broker~" + "a" * messages.SIGNATURE_CHARS)
        missing = await self.refused("broker", "nobody")
        self.assertEqual((404, "no running bottle you can reach as nobody"), (missing.status, str(missing)))
        self.assertEqual(404, forged.status)

    async def test_a_bottle_cant_message_itself(self) -> None:
        self.assertEqual(400, (await self.refused("broker", "broker")).status)
        self.assertEqual(400, (await self.refused(
            "app-1a2b3c4d-foo", messages.address("app-1a2b3c4d-foo", holder="app-1a2b3c4d-foo"))).status)

    async def test_a_bottle_that_isnt_running_is_not_found(self) -> None:
        self.assertEqual(404, (await self.refused("broker", "stopped")).status)

    async def test_no_session_taking_it_in_time_is_a_timeout(self) -> None:
        os.environ["FAKE_EXIT"] = str(daemon.NOT_TAKEN)
        refusal = await self.refused("broker", "app-1a2b3c4d-foo")
        self.assertEqual(504, refusal.status)
        self.assertIn("no session", str(refusal))

    async def test_any_other_failure_says_what_went_wrong(self) -> None:
        os.environ.update(FAKE_EXIT="127", FAKE_ERR="bottle-event: not found")
        refusal = await self.refused("broker", "app-1a2b3c4d-foo")
        self.assertEqual(502, refusal.status)
        self.assertIn("bottle-event: not found", str(refusal))

    async def test_an_exec_that_hangs_is_given_up_on(self) -> None:
        (self.tmp / "smolvm").write_text("#!/bin/sh\nexec sleep 5\n")
        with mock.patch.multiple(daemon, DELIVERY_TIMEOUT=0, DELIVERY_GRACE=0.2):
            self.assertEqual(504, (await self.refused("broker", "app-1a2b3c4d-foo")).status)

    async def test_list_bottles_shows_the_other_running_bottles_without_host_paths(self) -> None:
        self.assertEqual([{"name": "app-1a2b3c4d-bar", "repo": "app", "thread": "bar"},
                          {"name": "app-1a2b3c4d-foo", "repo": "app", "thread": "foo"}],
                         await self.daemon.peers("broker"))

    async def test_listing_bottles_needs_list_bottles(self) -> None:
        with self.assertRaises(daemon.ServiceError) as raised:
            await self.daemon.peers("app-1a2b3c4d-foo")
        self.assertEqual(403, raised.exception.status)


class RestoreTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        for patcher in (
            mock.patch.object(daemon.ca, "hosts", return_value=[]),
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon.cache, "cached_for", return_value=[]),
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
