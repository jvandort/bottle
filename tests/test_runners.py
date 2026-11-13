import unittest
from pathlib import Path
from unittest import mock

from bottle import repos, runners
from bottle.errors import BottleError
from tests.support import GitTestCase


class GetTest(unittest.TestCase):
    def test_local_is_the_default(self) -> None:
        self.assertIsInstance(runners.get(), runners.LocalRunner)
        self.assertIs(runners.get("local"), runners.get())

    def test_no_other_runner_yet(self) -> None:
        with self.assertRaisesRegex(BottleError, "no runner named 'box'"):
            runners.get("box")


class LocalRunnerTest(GitTestCase):
    """The local runner is bottle-cli, images and bottled here, as bottle used them before runners."""

    def test_starting_binds_its_machine_at_bottled_then_starts_it(self) -> None:
        calls = []
        with mock.patch.object(runners.daemon, "bind", side_effect=lambda *a: calls.append(("bind", *a))), \
                mock.patch.object(runners.krun, "start", side_effect=lambda *a: calls.append(("start", *a))):
            runners.get().start("b-machine", "b", (), ["tools"])
        self.assertEqual(calls, [("bind", "b", "b-machine", (), ["tools"]), ("start", "b-machine")])

    def test_a_machines_routes_are_to_bottleds_socket_here(self) -> None:
        policy = {"cache": [], "routes": [{"address": "192.0.2.1", "port": 80}]}
        with mock.patch.object(runners.krun, "create") as create, mock.patch.object(runners.krun, "configure") as configure:
            runners.get().create("m", Path("/stages/a.ext4"), [], {}, 512, network=policy)
            runners.get().configure("m", policy)
        routed = {"cache": [], "routes": [{"address": "192.0.2.1", "port": 80,
                                            "socket": str(runners.daemon.services_socket())}]}
        self.assertEqual(create.call_args.args[-1], routed)
        configure.assert_called_once_with("m", routed)

    def test_a_machine_mounts_the_repos_own_objects(self) -> None:
        path = self.make_repo("example")
        self.assertEqual(runners.get().objects(repos.Repo("example", path)), (path / ".git" / "objects").resolve())

    def test_mounts_are_bottle_clis_and_a_machine_has_all_the_cores(self) -> None:
        with mock.patch.object(runners.krun, "create") as create, mock.patch.object(runners.os, "cpu_count", return_value=12):
            runners.get().create("m", Path("/stages/a.ext4"), [runners.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                 {"A": "1"}, 512, 50)
        create.assert_called_once_with("m", "/stages/a.ext4", [runners.krun.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                       {"A": "1"}, 512, 12, 50, None, None, None, None)
