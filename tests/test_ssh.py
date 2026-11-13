import json
import os
import subprocess
import sys
from pathlib import Path
from unittest import mock

from bottle import bottles, prereqs, runners, ssh
from bottle.errors import BottleError
from tests.support import GitTestCase

# ssh, as far as bottle uses it: options up to --, the destination, and a command for its shell, run here
# as if at the destination, in a home of its own. "down" can't be reached. Whatever runs the tests, the
# destination is Linux on x86_64 (uname, from $FAKE_PATH), with the tests' own python3 (also from
# $FAKE_PATH), and its shell sees $FAKE_KVM where it says /dev/kvm, so a test says whether the machine has KVM.
FAKE_SSH = r"""#!/usr/bin/env bash
while [ "$1" != "--" ]; do shift; done
shift
destination=$1; shift
echo "$destination $*" >> "$FAKE_SSH_LOG"
if [ "$destination" = down ]; then
  echo "ssh: connect to host down port 22: Connection refused" >&2
  exit 255
fi
script=${1//\/dev\/kvm/$FAKE_KVM}
mkdir -p "$FAKE_REMOTES/$destination"
exec env -u BOTTLE_HOME HOME="$FAKE_REMOTES/$destination" PATH="$FAKE_PATH:$PATH" bash -c "$script"
"""

# bottle-cli there: every machine is running; an image build fails, saying what it was given.
FAKE_CLI = """#!/bin/sh
if [ "$1 $2" = "image build" ]; then
  echo "bottle-cli: can't build $3 ($(cat "$(dirname "$3")/context/install.sh" | head -c 9))" >&2
  exit 1
fi
echo '{"state": "running"}'
"""


class SshTestCase(GitTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.remotes = self.tmp / "remotes"
        self.log = self.tmp / "ssh.log"
        fake = self.tmp / "ssh"
        fake.write_text(FAKE_SSH)
        fake.chmod(0o755)
        self.kvm = self.tmp / "kvm"
        self.path = self.tmp / "remote-bin"
        self.path.mkdir()
        uname = self.path / "uname"
        uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) echo Linux ;; esac\n')
        uname.chmod(0o755)
        (self.path / "python3").symlink_to(sys.executable)
        env = mock.patch.dict(os.environ, {
            "BOTTLE_SSH": str(fake), "FAKE_SSH_LOG": str(self.log), "FAKE_REMOTES": str(self.remotes),
            "FAKE_KVM": str(self.kvm), "FAKE_PATH": str(self.path),
        })
        env.start()
        self.addCleanup(env.stop)
        # bottle-cli's distribution for x86_64 Linux, as xtask would build it here: a fake bottle-cli.
        self.built: list[str] = []
        self.dist = self.tmp / "dist"
        self.dist.mkdir()
        (self.dist / "bottle-cli").write_text(FAKE_CLI)
        (self.dist / "bottle-cli").chmod(0o755)
        (self.dist / prereqs.STAMP).write_text(prereqs.source_hash())
        patcher = mock.patch.object(prereqs, "ensure_dist", side_effect=lambda target: self.built.append(target)
                                    or self.dist)
        patcher.start()
        self.addCleanup(patcher.stop)

    def has_kvm(self) -> None:
        self.kvm.write_text("")

    def has_cli(self, destination: str = "box") -> Path:
        cli = self.remotes / destination / ".bottle" / "dist" / "bottle-cli"
        cli.parent.mkdir(parents=True)
        cli.write_text(FAKE_CLI)
        cli.chmod(0o755)
        return cli


    def logged(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []


class TransportTest(SshTestCase):
    def test_its_bottle_cli_is_what_runs_there_and_nothing_else_of_bottles(self) -> None:
        self.has_cli()
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        self.assertEqual(runner.state("bottle-b"), "running")
        self.assertEqual(len(self.logged()), 1)
        self.assertIn('"$HOME/.bottle/dist/bottle-cli" machine status --name bottle-b --json', self.logged()[0])
        self.assertFalse((self.remotes / "box" / ".bottle" / "runner").exists())

    def test_a_build_sends_its_context_with_the_recipe_and_builds_there(self) -> None:
        self.has_cli()
        context = self.tmp / "context"
        context.mkdir()
        (context / "install.sh").write_text("#!/bin/sh\n")
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        with mock.patch.object(runners.images.krun, "ca_certificate", return_value=None), \
                mock.patch("sys.stderr"), self.assertRaisesRegex(BottleError, "image build failed"):
            runner.build(context, "localhost/bottle/x:latest", "abc", {"pull": "debian", "steps": []})

    def test_a_runner_it_cant_reach_is_unreachable(self) -> None:
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "down"))
        with self.assertRaisesRegex(BottleError, r"runner box is unreachable \(ssh: connect to host down"):
            runner.state("bottle-b")

    def test_its_connection_is_shared_and_never_asks(self) -> None:
        command = ssh.SshTransport("box", "user@box.example").command(["bottle-cli", "machine", "exec"], tty=True)
        self.assertEqual(command[0], os.environ["BOTTLE_SSH"])
        for option in ("BatchMode=yes", "ControlMaster=auto", f"ControlPersist={ssh.PERSIST}"):
            self.assertIn(option, command)
        self.assertEqual(command[-4:], ["-t", "--", "user@box.example", "bottle-cli machine exec"])

    def test_a_shell_there_is_its_bottle_cli_exec_over_ssh(self) -> None:
        self.has_cli()
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        command = runner.exec_command("bottle-b", ["bash", "-l"], user="genie", workdir="/workspace", tty=True)
        self.assertEqual(command[-3:-1], ["--", "box"])
        self.assertTrue(command[-1].startswith('"$HOME/.bottle/dist/bottle-cli" machine exec --name bottle-b -t'))
        result = subprocess.run(command, capture_output=True, text=True)   # runs there
        self.assertEqual(json.loads(result.stdout), {"state": "running"})

    def test_what_it_has(self) -> None:
        self.has_kvm()
        check = ssh.SshTransport("box", "box").check()
        self.assertEqual((check.os, check.arch, check.kvm, check.git), ("Linux", "x86_64", True, True))
        self.assertEqual(check.missing(), [])
        self.kvm.unlink()
        self.assertFalse(ssh.SshTransport("box", "box").check().kvm)

    def test_no_route_on_a_mac_names_the_local_network_permission(self) -> None:
        why = "ssh: connect to host box.local port 22: No route to host"
        with mock.patch.object(ssh.platform, "system", return_value="Darwin"):
            self.assertIn("Local Network permission", ssh.explained(why))
            self.assertEqual(ssh.explained("ssh: Connection refused"), "ssh: Connection refused")
        with mock.patch.object(ssh.platform, "system", return_value="Linux"):
            self.assertEqual(ssh.explained(why), why)

    def test_what_it_lacks_is_said_as_its_fixed(self) -> None:
        check = ssh.Check("Darwin", "arm64", False, False)
        self.assertEqual(check.missing(), [
            "Linux (it runs Darwin)", "access to /dev/kvm (add the user to the kvm group)", "git",
        ])
        self.assertEqual(ssh.Check("Linux", "riscv64", True, True).missing(), ["x86_64 or arm64 (it's riscv64)"])


class RunnerCommandsTest(SshTestCase):
    def test_add_checks_and_records_it(self) -> None:
        self.has_kvm()
        check = runners.add("box", "box")
        self.assertEqual(check.arch, "x86_64")
        self.assertEqual(runners.registered(), {"box": {"ssh": "box"}})
        self.assertIsInstance(runners.get("box"), runners.RemoteRunner)

    def test_add_refuses_what_bottle_cant_set_up(self) -> None:
        with self.assertRaisesRegex(BottleError, r"box can't be a runner yet; it needs: access to /dev/kvm"):
            runners.add("box", "box")
        self.assertEqual(runners.registered(), {})

    def test_add_copies_bottle_cli_built_here_for_its_architecture(self) -> None:
        self.has_kvm()
        runners.add("box", "box")
        self.assertEqual(self.built, ["x86_64-unknown-linux-musl"])
        there = self.remotes / "box" / ".bottle" / "dist"
        self.assertEqual((there / "bottle-cli").read_text(), FAKE_CLI)
        self.assertEqual((there / prereqs.STAMP).read_text(), prereqs.source_hash())
        runner = runners.get("box")
        self.assertEqual(runner.state("bottle-b"), "running")  # what's there is what it runs

    def test_its_bottle_cli_is_copied_again_only_when_its_sources_change(self) -> None:
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        runner.ensure_ready()
        there = self.remotes / "box" / ".bottle" / "dist"
        (there / "left-by-the-old").write_text("")
        runner.ensure_ready()
        self.assertEqual(len(self.built), 1)  # current: neither built nor copied
        (there / prereqs.STAMP).write_text("other sources")
        runner.ensure_ready()
        self.assertEqual(len(self.built), 2)
        self.assertFalse((there / "left-by-the-old").exists())  # replaced whole

    def test_an_architecture_without_a_build_is_refused(self) -> None:
        (self.path / "uname").write_text('#!/bin/sh\ncase "$1" in -m) echo riscv64 ;; *) echo Linux ;; esac\n')
        with self.assertRaisesRegex(BottleError, "runner box is riscv64: bottle-cli is built for x86_64 and arm64"):
            runners.RemoteRunner("box", ssh.SshTransport("box", "box")).ensure_ready()

    def test_add_refuses_a_name_thats_taken_by_another_machine_or_local(self) -> None:
        runners.register("box", "elsewhere")
        with self.assertRaisesRegex(BottleError, "already elsewhere"):
            runners.add("box", "box")
        for name in ("local", "a b", "-x"):
            with self.subTest(name), self.assertRaisesRegex(BottleError, "invalid runner name"):
                runners.add(name, "box")

    def test_unreachable_isnt_added(self) -> None:
        with self.assertRaisesRegex(BottleError, "unreachable"):
            runners.add("box", "down")

    def test_status_says_which_are_reachable_and_ready(self) -> None:
        self.has_kvm()
        runners.register("box", "box")
        runners.register("off", "down")
        rows = {row["name"]: row for row in runners.status()}
        self.assertEqual(list(rows), ["local", "box", "off"])
        self.assertEqual((rows["box"]["reachable"], rows["box"]["os"], rows["box"]["missing"]), (True, "Linux", []))
        self.assertFalse(rows["off"]["reachable"])
        self.assertIn("unreachable", rows["off"]["error"])
        self.assertEqual(runners.status(["gone"]), [{"name": "gone", "bottles": [], "state": "missing"}])

    def test_remove_forgets_a_runner_without_bottles(self) -> None:
        runners.register("box", "box")
        with mock.patch.object(bottles, "load", return_value={"b": mock.Mock(runner="box")}) as load:
            load.return_value["b"].name = "b"
            with self.assertRaisesRegex(BottleError, "runner box has bottles: b"):
                runners.remove("box")
        runners.remove("box")
        self.assertEqual(runners.registered(), {})
        with self.assertRaisesRegex(BottleError, "no runner named"):
            runners.remove("box")
        with self.assertRaisesRegex(BottleError, "can't be removed"):
            runners.remove("local")
