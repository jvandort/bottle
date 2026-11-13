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

    def test_a_machines_routes_are_to_folio_serves_sockets_here(self) -> None:
        policy = {"cache": [], "routes": [{"address": "192.0.2.1", "port": 80, "service": "host"}]}
        with mock.patch.object(runners.krun, "create") as create, mock.patch.object(runners.krun, "configure") as configure:
            runners.get().create("m", "a", [], {}, 512, network=policy)
            runners.get().configure("m", policy)
        routed = {"cache": [], "routes": [{"address": "192.0.2.1", "port": 80,
                                            "socket": str(runners.daemon.services_sockets()["host"])}]}
        self.assertEqual(create.call_args.args[-1], routed)
        configure.assert_called_once_with("m", routed, cli=runners.krun.LOCAL)

    def test_a_machine_mounts_the_repos_own_objects(self) -> None:
        path = self.make_repo("example")
        self.assertEqual(runners.get().objects(repos.Repo("example", path)), (path / ".git" / "objects").resolve())

    def test_mounts_are_bottle_clis_and_a_machine_has_all_the_cores(self) -> None:
        with mock.patch.object(runners.krun, "create") as create, mock.patch.object(runners.os, "cpu_count", return_value=12):
            runners.get().create("m", "a", [runners.krun.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                 {"A": "1"}, 512, 50)
        create.assert_called_once_with("m", "a", [runners.krun.Mount(Path("/repo/objects"), "/mnt/objects/w")],
                                       {"A": "1"}, 512, 12, 50, None, None, None, None, cli=runners.krun.LOCAL)


class RemoteRunnerTest(unittest.TestCase):
    """A runner elsewhere is folio-cli's: its bottle-cli there, through `folio-cli on`, its machines by their names
    there; their routes to folio serve's sockets there, which folio serve forwards here."""

    def test_its_machines_are_its_bottle_clis_by_their_names_there(self) -> None:
        box = runners.Runner("box")
        with mock.patch.object(runners.krun, "start") as start, \
                mock.patch.object(runners.krun, "state", return_value="running") as state:
            box.start("m")
            self.assertEqual(box.state("m"), "running")
            self.assertEqual(box.cli.command(["images", "--json"])[1:], ["on", "box", "images", "--json"])
            command = box.exec_command("m", ["bash", "-l"], user="genie", tty=True)
        start.assert_called_once_with("m", cli=box.cli)
        state.assert_called_once_with("m", cli=box.cli)
        self.assertEqual(command[1:6], ["on", "box", "exec", "m", "-t"])

    def test_its_machines_routes_are_to_sockets_there_forwarded_here(self) -> None:
        box = runners.Runner("box")
        with mock.patch.object(runners, "home", return_value="/home/me"):
            there = box.services_sockets()
            forwards = box.forwards()
        key = runners.controller_key()
        here = runners.daemon.services_sockets()
        self.assertEqual(there["host"], f"/home/me/.folio/services/{key}/host.sock")
        self.assertEqual(set(there), set(here))
        self.assertIn(f"/home/me/.folio/services/{key}/ssh.sock={here['ssh']}", forwards)


class RunnerCommandTest(GitTestCase):
    """`folio runner` is folio-cli's, but for removing a runner folio still has bottles on."""

    def test_a_runner_with_bottles_isnt_removed(self) -> None:
        from folio import cli
        from folio.bottle import bottles
        from tests.bottle.support import write_bottles

        write_bottles({"b": bottles.Bottle("b", "i", "base", 0.0, status="ready", runner="box", machine_name="m")})
        with mock.patch("os.execvp") as execvp, mock.patch("sys.stderr") as stderr:
            self.assertEqual(cli.main(["runner", "rm", "box"]), 1)
            execvp.assert_not_called()
            said = "".join(call.args[0] for call in stderr.write.call_args_list)
        self.assertIn("runner box has bottles: b", said)
        with mock.patch("os.execvp") as execvp:
            cli.main(["runner", "ls", "--json"])
        self.assertEqual(execvp.call_args.args[1][1:], ["runner", "ls", "--json"])
