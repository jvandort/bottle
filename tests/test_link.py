import asyncio
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import daemon, egress, host, link, runners
from bottle.decider import Decider
from bottle.errors import BottleError
from tests.support import GitTestCase, run
from tests.test_host import serve_bottle_host


class Echoing:
    """A controller's gateway for a bottle whose credential's flows it answers by echoing, after a greeting."""

    def __init__(self) -> None:
        self.flows: list[egress.Flow] = []

    async def serve_credential(self, flow, reader, writer) -> None:
        self.flows.append(flow)
        writer.write(b"hello " + flow.first)
        writer.write(await reader.read(100))
        await writer.drain()
        writer.close()


class LinkTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """A runner's bottled and its controller's, talking over the link: a socket here, as ssh would forward it."""

    def setUp(self) -> None:
        super().setUp()
        for patcher in (mock.patch.object(daemon.ca, "hosts", return_value=["ci.test"]),
                        mock.patch.object(daemon.cache, "cached_for", return_value=[])):
            patcher.start()
            self.addCleanup(patcher.stop)
        short = Path(tempfile.mkdtemp(dir="/tmp"))   # macOS caps Unix socket paths at 104 bytes
        self.addCleanup(shutil.rmtree, short, ignore_errors=True)
        self.socket = short / "link.sock"

    async def link_up(self, controller) -> link.LinkServer:
        server = link.LinkServer("box", controller)
        listening = await asyncio.start_unix_server(server.handle, str(self.socket))
        self.addAsyncCleanup(self._close, listening)
        return server

    @staticmethod
    async def _close(server) -> None:
        server.close()
        await server.wait_closed()

    def runner(self) -> daemon.Daemon:
        return daemon.Daemon(decider=Decider(self.tmp / "tokens.json"), decider_port=0,
                             controller=link.LinkController(self.socket))

    def controller(self) -> daemon.Controller:
        return daemon.Controller(egress.Policy(), running=lambda bottle: False)

    async def test_binding_there_makes_the_bottles_side_here_and_learns_only_where_its_credentials_are_for(self):
        injections = [egress.Injection("ci.test", "Authorization", "Bearer the-real-credential", feature="teamcity")]
        with mock.patch.object(daemon.auth, "injections_for", return_value=injections):
            server = await self.link_up(self.controller())
            runner = self.runner()
            await runner.bind("b", (), ("teamcity",))
        self.assertTrue(server.bound("b"))
        [binding] = runner.decider.bindings.values()
        self.assertEqual([(i.host, i.header, i.value, i.feature) for i in binding.gateway.injections],
                         [("ci.test", "Authorization", "", "teamcity")])
        await runner.release("b")
        self.assertFalse(server.bound("b"))

    async def test_bottle_host_there_is_answered_here(self):
        upstream = self.make_repo("upstream")
        repo = self.tmp / "repo"
        run("git", "clone", "-q", upstream, repo)
        with mock.patch.object(daemon.auth, "injections_for", return_value=[]):
            await self.link_up(self.controller())
            served = (host.Served(repo / ".git", (host.Remote("origin", "upstream"),), view_dir=self.tmp / "view.git"),)
            _, services, _ = await link.LinkController(self.socket).egress("b", served, ())
        server, env = await serve_bottle_host(services)
        self.addAsyncCleanup(self._close, server)
        workspace = self.tmp / "workspace"
        run("git", "init", "-q", workspace)
        new = self.commit(upstream, "upstream moved")   # pulled through, on the controller
        result = await asyncio.to_thread(subprocess.run, ["git", "-C", workspace, "fetch", "-q", "http://bottle.host/git/origin",
                                                          "main:origin-main"], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(run("git", "-C", workspace, "rev-parse", "origin-main"), new)

    async def test_bottle_host_with_the_link_down_says_so(self):
        services = link._LinkedServices(link.LinkController(self.socket), "b")
        server, env = await serve_bottle_host(services)
        self.addAsyncCleanup(self._close, server)
        result = await asyncio.to_thread(subprocess.run, ["git", "ls-remote", "http://bottle.host/git/origin"], env=env,
                                         capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("503", result.stderr)

    async def test_a_credentials_flow_there_is_served_here(self):
        echoing = Echoing()
        controller = mock.Mock()

        async def egress_for(bottle, repos, features):
            return (), host.HostServices(), echoing

        controller.egress = egress_for
        await self.link_up(controller)
        _, _, gateway = await link.LinkController(self.socket).egress("b", (), ())
        answered = asyncio.Queue()

        async def bottle_side(reader, writer):
            await gateway.serve_credential(egress.Flow("203.0.113.5", 443, b"\x16hello"), reader, writer)

        guest = await asyncio.start_server(bottle_side, "127.0.0.1", 0)
        self.addAsyncCleanup(self._close, guest)
        reader, writer = await asyncio.open_connection("127.0.0.1", guest.sockets[0].getsockname()[1])
        writer.write(b"and more")
        await writer.drain()
        self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"hello \x16helloand more")
        self.assertEqual(echoing.flows, [egress.Flow("203.0.113.5", 443, b"\x16hello")])

    async def test_a_bottle_bound_from_no_runner_is_served_nothing(self):
        await self.link_up(self.controller())
        with self.assertRaisesRegex(BottleError, "no bottle 'b' bound from runner box"):
            await link.LinkController(self.socket)._ask({"op": "host", "bottle": "b", "method": "GET", "path": "/"})

    async def test_with_the_link_down_a_bottle_cant_be_bound_until_it_comes_up(self):
        Decider(self.tmp / "tokens.json").bind("b", None)   # its machine holds the token from before
        runner = self.runner()
        runner.remembered = self.tmp / "bindings.json"
        with self.assertRaisesRegex(BottleError, "the controller can't be reached"):
            await runner.bind("x", (), ())
        runner._remember("b", ("b", [], ["tools"]))
        self.assertEqual(runner.remembered_bindings(), [("b", (), ("tools",))])
        self.assertEqual(await runner.restore(runner.remembered_bindings()), [("b", (), ("tools",))])
        with mock.patch.object(daemon.auth, "injections_for", return_value=[]):
            await self.link_up(self.controller())
            self.assertEqual(await runner.restore(runner.remembered_bindings()), [])
        self.assertTrue(runner.decider.bound("b"))
        await runner.release("b")
        self.assertEqual(runner.remembered_bindings(), [])


class LinksTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """The controller keeping its link to a runner: ssh forwarding the runner's link.sock to a socket here."""

    def setUp(self) -> None:
        super().setUp()
        short = Path(tempfile.mkdtemp(dir="/tmp"))
        self.addCleanup(shutil.rmtree, short, ignore_errors=True)
        self.remotes = short / "r"
        fake = self.tmp / "ssh"
        # A forward is a symlink to the socket here, held as long as this ssh runs.
        fake.write_text(r"""#!/usr/bin/env bash
forward=
while [ "$1" != "--" ]; do [ "$1" = -R ] && forward=$2; shift; done
shift; destination=$1; shift
[ "$destination" = down ] && { echo "ssh: connect to host down port 22: Connection refused" >&2; exit 255; }
mkdir -p "$FAKE_REMOTES/$destination"
if [ -n "$forward" ]; then ln -sfn "${forward#*:}" "${forward%%:*}"; exec sleep 3600; fi
exec env -u BOTTLE_HOME HOME="$FAKE_REMOTES/$destination" bash -c "$1"
""")
        fake.chmod(0o755)
        env = mock.patch.dict(os.environ, {"BOTTLE_SSH": str(fake), "FAKE_REMOTES": str(self.remotes),
                                           "BOTTLE_HOME": str(short / "home")})
        env.start()
        self.addCleanup(env.stop)

    async def test_its_link_comes_up_and_reaches_the_controller(self):
        runners.register("box", "box")
        controller = daemon.Controller(egress.Policy(), running=lambda b: False)
        self.addAsyncCleanup(controller.links.close)
        await controller.links.ensure("box")
        there = self.remotes / "box" / ".bottle" / "link.sock"
        reply, _ = await link.LinkController(there)._ask({"op": "release", "bottle": "b"})
        self.assertEqual(reply["ok"], True)

    async def test_a_runner_it_cant_reach_is_unreachable(self):
        runners.register("off", "down")
        controller = daemon.Controller(egress.Policy(), running=lambda b: False)
        self.addAsyncCleanup(controller.links.close)
        with mock.patch.object(daemon, "LINK_TIMEOUT", 1), \
                self.assertRaisesRegex(BottleError, r"runner off is unreachable: its link didn't come up \(runner off is unreachable"):
            await controller.links.ensure("off")

    async def test_a_link_that_ends_comes_back(self):
        runners.register("box", "box")
        controller = daemon.Controller(egress.Policy(), running=lambda b: False)
        self.addAsyncCleanup(controller.links.close)
        await controller.links.ensure("box")
        controller.links.processes["box"].terminate()
        for _ in range(100):
            if not controller.links.up["box"].is_set():
                break
            await asyncio.sleep(0.05)
        await controller.links.ensure("box")   # up again, after its backoff
        self.assertTrue(controller.links.up["box"].is_set())
