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


class SocketPathTest(unittest.TestCase):
    def test_lives_in_folios_home(self) -> None:
        with mock.patch.dict(os.environ, {"FOLIO_HOME": "/Users/me/.folio"}):
            self.assertEqual(daemon.socket_path(), Path("/Users/me/.folio/serve.sock"))
            self.assertEqual(daemon.services_sockets(), {name: Path(f"/Users/me/.folio/services/{name}.sock")
                                                         for name in ("host", "ssh", "credentials")})

    def test_deep_home_gets_a_short_path(self) -> None:
        deep = "/Users/me/" + "x" * 120
        with mock.patch.dict(os.environ, {"FOLIO_HOME": deep}):
            path = daemon.socket_path()
        self.assertLessEqual(len(str(path)), daemon.MAX_SOCKET_PATH)
        self.assertTrue(str(path).startswith(f"/tmp/bottle-{os.getuid()}/"))


class OpenFilesTest(unittest.TestCase):
    """bottled raises its limit on open files: macOS's 256 runs out under a few builds at once."""

    def limits(self, soft: int, hard: int, most: int | None = None):
        self.set: list[tuple[int, int]] = []

        def setrlimit(_, limits):
            if most is not None and limits[0] > most:
                raise ValueError("not allowed to raise maximum limit")
            self.set.append(limits)

        return mock.patch.multiple(daemon.resource, getrlimit=mock.Mock(return_value=(soft, hard)), setrlimit=setrlimit)

    def test_up_to_what_it_wants(self) -> None:
        with self.limits(256, daemon.resource.RLIM_INFINITY):
            self.assertEqual(daemon.raise_open_files(), daemon.OPEN_FILES)
        self.assertEqual(self.set, [(daemon.OPEN_FILES, daemon.resource.RLIM_INFINITY)])

    def test_no_higher_than_the_hard_limit(self) -> None:
        with self.limits(256, 4096):
            self.assertEqual(daemon.raise_open_files(), 4096)

    def test_as_high_as_the_system_takes(self) -> None:
        # macOS: an unlimited hard limit, and kern.maxfilesperproc below what's asked for.
        with self.limits(256, daemon.resource.RLIM_INFINITY, most=24576):
            self.assertEqual(daemon.raise_open_files(), 16384)

    def test_never_lowered(self) -> None:
        with self.limits(100000, daemon.resource.RLIM_INFINITY):
            self.assertEqual(daemon.raise_open_files(), 100000)
        self.assertEqual(self.set, [])


class DaemonTest(unittest.IsolatedAsyncioTestCase):
    """bottled itself; what it does with what a machine routes to it is test_services' and test_egress'."""

    async def asyncSetUp(self) -> None:
        # Keep the socket path short: macOS caps Unix socket paths at 104 bytes.
        tmp = tempfile.mkdtemp(dir="/tmp")
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        self.tmp = Path(tmp)
        self.path = self.tmp / "d.sock"
        self.services = self.tmp / "host.sock"
        self.daemon = Daemon(services_paths={n: self.tmp / f"{n}.sock" for n in daemon.SERVICES})
        for patcher in (
            # The features these tests name aren't resolved: what a credential claims is mocked where it matters.
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
        self.assertEqual(await self.request({"op": "ping"}), {"ok": True, "code": daemon.code_hash()})

    async def test_sockets_are_owner_only(self) -> None:
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.services.stat().st_mode & 0o777, 0o600)

    async def test_a_bottles_credentials_reach_the_controllers_gateway_and_go_no_further(self) -> None:
        injections = [daemon.egress.Injection("ci.test", "Authorization", "Bearer the-real-credential")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=injections) as looked_up:
            reply = await self.request({"op": "bind", "bottle": "b", "machine": "b-m",
                                        "features": ["teamcity:server=ci.test"]})
        looked_up.assert_called_once_with(("teamcity:server=ci.test",))
        self.assertNotIn("the-real-credential", json.dumps(reply))
        self.assertEqual(list(self.daemon.services.bindings), ["b-m"])  # by its machine
        [binding] = self.daemon.services.bindings.values()
        # The runner's gateway knows where the credential is for, and hands those flows to the controller's.
        self.assertEqual([(i.host, i.port, i.value) for i in binding.gateway.injections], [("ci.test", 443, "")])
        self.assertEqual(binding.gateway.hand_off.__self__.injections, tuple(injections))

    async def test_binding_again_reads_the_credentials_again(self) -> None:
        for value in ("Bearer old", "Bearer new"):
            injections = [daemon.egress.Injection("ci.test", "Authorization", value)]
            with mock.patch.object(daemon.auth, "injections_for", return_value=injections):
                await self.request({"op": "bind", "bottle": "b", "machine": "b-m", "features": ["teamcity"]})
        [binding] = self.daemon.services.bindings.values()
        self.assertEqual(binding.gateway.hand_off.__self__.injections[0].value, "Bearer new")

    async def test_shutdown_removes_the_sockets(self) -> None:
        await self.request({"op": "bind", "bottle": "b", "machine": "b-m"})
        self.assertEqual(await self.request({"op": "shutdown"}), {"ok": True})
        await asyncio.wait_for(self.task, 2)
        self.assertFalse(self.path.exists())
        self.assertFalse(self.services.exists())

    async def test_client_shutdown_when_not_running(self) -> None:
        with mock.patch.object(daemon, "socket_path", return_value=self.path.with_name("absent.sock")):
            self.assertFalse(await asyncio.to_thread(daemon.stop))

    async def test_unknown_op(self) -> None:
        self.assertEqual(await self.request({"op": "nope"}), {"ok": False, "error": "unknown op 'nope'"})

    async def test_second_daemon_exits(self) -> None:
        await asyncio.wait_for(daemon.serve(self.path, Daemon(services_paths={"host": self.tmp / "s2.sock"})), 2)


class DeliverTest(unittest.IsolatedAsyncioTestCase):
    """Daemon.deliver and Daemon.peers: messages between bottles, run in the recipient by way of a fake bottle-cli."""

    async def asyncSetUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        # Records its arguments and stdin, and exits as $FAKE_EXIT says.
        smolvm = self.tmp / "smolvm"
        smolvm.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$FAKE_DIR/argv"\ncat > "$FAKE_DIR/stdin"\n'
                          'echo "$FAKE_ERR" >&2\nexit "${FAKE_EXIT:-0}"\n')
        smolvm.chmod(0o755)
        env = {"BOTTLE_CLI": str(smolvm), "FAKE_DIR": str(self.tmp), "FAKE_ERR": "", "BOTTLE_HOME": str(self.tmp)}
        patcher = mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)
        for patcher in (
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon.cache, "cached_for", return_value=[]),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.daemon = Daemon()

        def bottle(name: str, repo: str | None = None, **spec) -> mock.Mock:
            return SimpleNamespace(machine=f"bottle-{name}", mounts=[SimpleNamespace(repo=repo)] if repo else [], spec=spec,
                                   runner="local")

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
            await self.daemon.bind(name, f"bottle-{name}")

    def ran(self) -> tuple[list[str], str]:
        """What the fake bottle-cli ran last, after `--`, and in which machine; and its stdin."""
        argv = (self.tmp / "argv").read_text().splitlines()
        return argv[argv.index("--name") + 1], argv[argv.index("--") + 1:], (self.tmp / "stdin").read_text()

    async def refused(self, sender: str, to: str) -> daemon.ServiceError:
        with self.assertRaises(daemon.ServiceError) as raised:
            await self.daemon.controller.deliver(sender, to, "hi")
        return raised.exception

    async def test_a_bottle_with_message_any_sends_by_name_from_an_address_issued_to_the_recipient(self) -> None:
        await self.daemon.controller.deliver("broker", "app-1a2b3c4d-foo", "status?\nall of it")
        machine, argv, stdin = self.ran()
        self.assertEqual("bottle-app-1a2b3c4d-foo", machine)
        reply_to = messages.address("broker", holder="app-1a2b3c4d-foo")
        self.assertEqual([daemon.BOTTLE_EVENT, "send", "-t", str(daemon.DELIVERY_TIMEOUT), "-f", reply_to], argv)
        self.assertEqual("status?\nall of it", stdin)

    async def test_the_recipient_answers_by_the_address_it_was_given(self) -> None:
        await self.daemon.controller.deliver("app-1a2b3c4d-foo", messages.address("broker", holder="app-1a2b3c4d-foo"), "done")
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
        # Their labels; one recorded before labels, its thread as the label thread.
        self.assertEqual([{"name": "app-1a2b3c4d-bar", "repo": "app", "labels": {"thread": "bar"}},
                          {"name": "app-1a2b3c4d-foo", "repo": "app", "labels": {"thread": "foo"}}],
                         await self.daemon.controller.peers("broker"))

    async def test_listing_bottles_needs_list_bottles(self) -> None:
        with self.assertRaises(daemon.ServiceError) as raised:
            await self.daemon.controller.peers("app-1a2b3c4d-foo")
        self.assertEqual(403, raised.exception.status)


class RestoreTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        for patcher in (
            mock.patch.object(daemon.auth, "injections_for", return_value=[]),
            mock.patch.object(daemon.cache, "cached_for", return_value=[]),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    async def test_binds_each_by_its_machine_unless_its_bound_already(self) -> None:
        d = Daemon()
        await d.bind("b", "b-m")
        bound = d.services.bindings["b-m"]
        await d.restore([("b", "b-m", (), ()), ("other", "other-m", (), ())])
        self.assertEqual({m: b.bottle for m, b in d.services.bindings.items()}, {"b-m": "b", "other-m": "other"})
        self.assertIs(d.services.bindings["b-m"], bound)

    async def test_one_failure_doesnt_stop_the_rest(self) -> None:
        d = Daemon()
        real = d.controller.egress

        async def egress(bottle, *args):
            if bottle == "bad":
                raise OSError("its repo is gone")
            return await real(bottle, *args)

        with mock.patch.object(d.controller, "egress", side_effect=egress), self.assertLogs("bottle.daemon", "WARNING"):
            unrestored = await d.restore([("bad", "bad-m", (), ()), ("good", "good-m", (), ())])
        self.assertEqual([binding.bottle for binding in d.services.bindings.values()], ["good"])
        self.assertEqual([b[0] for b in unrestored], ["bad"])


class CurrentCodeTest(unittest.TestCase):
    """A command finds bottled running its own code, or restarts it first: once a command."""

    def setUp(self) -> None:
        patcher = mock.patch.object(daemon, "_current", False)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.calls: list[str] = []
        self.running = {"code": "old"}

        self.closing = 0   # pings a stopping one takes and closes unanswered

        def send(message, timeout=30):
            self.calls.append(message["op"])
            if self.running is None:
                if self.closing:
                    self.closing -= 1
                    raise daemon._Closed
                raise FileNotFoundError
            if message["op"] == "shutdown":
                self.running = None
                return {"ok": True}
            return {"ok": True, **self.running}

        def start_daemon():
            self.calls.append("start")
            self.running = {"code": daemon.code_hash()}

        for patcher in (mock.patch.object(daemon, "_send", side_effect=send),
                        mock.patch.object(daemon, "_start_daemon", side_effect=start_daemon),
                        mock.patch("sys.stderr")):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_one_on_other_code_is_restarted_before_its_asked_anything(self) -> None:
        daemon.bind("b", "b-m")
        self.assertEqual(self.calls, ["ping", "shutdown", "ping", "start", "bind"])
        daemon.release_egress("b")
        self.assertEqual(self.calls[-1:], ["release"])   # asked once a command, not once a request

    def test_one_that_closes_connections_as_it_stops_is_waited_out(self) -> None:
        self.closing = 2
        with mock.patch("time.sleep"):
            daemon.bind("b", "b-m")
        self.assertEqual(self.calls, ["ping", "shutdown", "ping", "ping", "ping", "start", "bind"])

    def test_one_from_before_it_said_its_code_is_restarted(self) -> None:
        self.running = {}
        self.assertTrue(daemon.start())   # started: the one that was running is gone
        self.assertEqual(self.calls, ["ping", "ping", "shutdown", "ping", "start"])

    def test_one_on_this_code_is_left_alone(self) -> None:
        self.running = {"code": daemon.code_hash()}
        daemon.bind("b", "b-m")
        self.assertEqual(self.calls, ["ping", "bind"])

    def test_none_running_is_started_by_what_needs_it(self) -> None:
        self.running = None
        daemon.bind("b", "b-m")
        self.assertEqual(self.calls, ["ping", "bind", "start", "bind"])
