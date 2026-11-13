import unittest
from pathlib import Path
from unittest import mock

from folio.bottle import repos, runners
from folio.bottle.errors import BottleError
from tests.bottle.support import GitTestCase


class GetTest(unittest.TestCase):
    def test_local_is_the_default(self) -> None:
        self.assertEqual(runners.get().name, "local")
        self.assertIs(runners.get("local"), runners.get())

    def test_no_other_runner_yet(self) -> None:
        with mock.patch.object(runners, "registered", return_value={}), \
                self.assertRaisesRegex(BottleError, "no runner named 'box'"):
            runners.get("box")


class LocalRunnerTest(GitTestCase):
    """The local runner is bottle-cli here."""

    def setUp(self) -> None:
        super().setUp()
        patcher = mock.patch.object(runners.krun.prereqs, "ensure_cli")   # its bottle-cli as it is
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_a_machines_routes_are_to_folio_serves_sockets_here(self) -> None:
        policy = {"cache": [], "routes": [{"address": "192.0.2.1", "port": 80, "service": "host"}]}
        with mock.patch.object(runners.krun, "create") as create, mock.patch.object(runners.krun, "configure") as configure:
            runners.get().create("m", "a", [], {}, 512, network=policy)
            runners.get().configure("m", policy)
        routed = {"cache": [], "routes": [{"address": "192.0.2.1", "port": 80,
                                            "socket": str(runners.daemon.services_sockets()["host"])}]}
        self.assertEqual(create.call_args.args[-1], routed)
        configure.assert_called_once_with("m", routed)

    def test_a_machine_mounts_the_repos_own_objects(self) -> None:
        path = self.make_repo("example")
        self.assertEqual(runners.get().objects(repos.Repo("example", path)), (path / ".git" / "objects").resolve())

    def test_mounts_are_bottle_clis_and_a_machine_has_all_the_cores(self) -> None:
        with mock.patch.object(runners.krun, "create") as create, mock.patch.object(runners.os, "cpu_count", return_value=12):
            runners.get().create("m", "a", [runners.krun.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                 {"A": "1"}, 512, 50)
        create.assert_called_once_with("m", "a", [runners.krun.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                       {"A": "1"}, 512, 12, 50, None, None, None, None)


class RemoteRunnerTest(unittest.TestCase):
    """A runner elsewhere is bottle-cli's: its machines by RUNNER/NAME, its images and CAs bottle-cli's there."""

    def test_its_machines_are_named_runner_slash_name_its_images_asked_there(self) -> None:
        box = runners.Runner("box")
        with mock.patch.object(runners.krun, "start") as start, \
                mock.patch.object(runners.krun, "state", return_value="running") as state, \
                mock.patch.object(runners.krun.prereqs, "ensure_cli"):
            box.start("m")
            self.assertEqual(box.state("m"), "running")
            self.assertEqual(box.cli.command(["images", "/s", "--json"])[1:], ["on", "box", "images", "/s", "--json"])
            command = box.exec_command("m", ["bash", "-l"], user="genie", tty=True)
        start.assert_called_once_with("box/m")
        state.assert_called_once_with("box/m")
        self.assertEqual(command[1:4], ["exec", "box/m", "-t"])
