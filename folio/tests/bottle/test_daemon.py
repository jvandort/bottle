"""folio.bottle.daemon: how folio finds and talks to folio serve. What folio serve does is folio-cli's (its
tests/serve.rs)."""

import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from folio.bottle import daemon


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


class NotRunningTest(unittest.TestCase):
    def test_stopping_one_that_isnt_running_says_so(self) -> None:
        with tempfile.TemporaryDirectory(dir="/tmp") as home, mock.patch.dict(os.environ, {"FOLIO_HOME": home}):
            self.assertFalse(daemon.stop())

    def test_releasing_from_one_that_isnt_running_starts_nothing(self) -> None:
        with tempfile.TemporaryDirectory(dir="/tmp") as home, mock.patch.dict(os.environ, {"FOLIO_HOME": home}), \
                mock.patch.object(daemon, "_start_daemon", side_effect=AssertionError("started")):
            daemon.release_egress("b")


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
