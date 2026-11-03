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
    """The local runner is smolvm, images and bottled here, as bottle used them before runners."""

    def test_starting_binds_at_bottleds_decider_then_starts_with_its_token(self) -> None:
        calls = []
        with mock.patch.object(runners.daemon, "bind_decider",
                               side_effect=lambda *a: calls.append(("bind", *a)) or ("127.0.0.1:1", "t")), \
                mock.patch.object(runners.smolvm, "start", side_effect=lambda *a: calls.append(("start", *a))):
            runners.get().start("bottle-b", "b", (), ["tools"])
        self.assertEqual(calls, [("bind", "b", (), ["tools"]), ("start", "bottle-b", "127.0.0.1:1", "t")])

    def test_a_machine_mounts_the_repos_own_objects(self) -> None:
        path = self.make_repo("example")
        self.assertEqual(runners.get().objects(repos.Repo("example", path)), (path / ".git" / "objects").resolve())

    def test_mounts_are_smolvms_and_a_machine_has_all_the_cores(self) -> None:
        with mock.patch.object(runners.smolvm, "create") as create, mock.patch.object(runners.os, "cpu_count", return_value=12):
            runners.get().create("m", Path("/images/a.tar"), [runners.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                 {"A": "1"}, 512, 50)
        create.assert_called_once_with("m", "/images/a.tar", [runners.smolvm.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                       {"A": "1"}, 512, 12, 50)
