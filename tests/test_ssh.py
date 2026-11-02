import json
import os
import subprocess
from pathlib import Path
from unittest import mock

from bottle import bottles, runners, ssh
from bottle.errors import BottleError
from tests.support import GitTestCase

# ssh, as far as bottle uses it: options up to --, the destination, and a command for its shell, run here
# as if at the destination, in a home of its own. "down" can't be reached. Its shell sees $FAKE_KVM
# where it says /dev/kvm, so a test says whether the machine has KVM.
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

# smolvm there: every machine is running.
FAKE_SMOLVM = """#!/bin/sh
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
        env = mock.patch.dict(os.environ, {
            "BOTTLE_SSH": str(fake), "FAKE_SSH_LOG": str(self.log), "FAKE_REMOTES": str(self.remotes),
            "FAKE_KVM": str(self.kvm), "FAKE_PATH": str(self.path),
        })
        env.start()
        self.addCleanup(env.stop)

    def has_kvm(self) -> None:
        self.kvm.write_text("")

    def has_smolvm(self, destination: str = "box") -> Path:
        smolvm = self.remotes / destination / ".bottle" / "smolvm" / "current" / "smolvm"
        smolvm.parent.mkdir(parents=True)
        smolvm.write_text(FAKE_SMOLVM)
        smolvm.chmod(0o755)
        return smolvm

    def logged(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []


class TransportTest(SshTestCase):
    def test_its_code_is_sent_the_first_time_and_run_there_after(self) -> None:
        self.has_smolvm()
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        self.assertEqual(runner.state("bottle-b"), "running")
        code = self.remotes / "box" / ".bottle" / "runner" / ssh.code_hash()
        self.assertTrue((code / ".complete").exists() and (code / "bottle" / "remote.py").exists())
        self.assertEqual(len(self.logged()), 3)  # asked, sent, asked again
        runner.state("bottle-b")
        self.assertEqual(len(self.logged()), 4)  # just asked

    def test_a_build_streams_its_context_there_and_its_answer_back(self) -> None:
        context = self.tmp / "context"
        context.mkdir()
        (context / "Dockerfile").write_text("FROM scratch")
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        # No smolvm there: the build fails there, before any machine, and says why, here.
        with mock.patch.object(ssh.SshTransport, "link"), mock.patch.dict(os.environ, {"BOTTLE_SMOLVM": "/no/smolvm"}), \
                self.assertRaisesRegex(BottleError, r"runner box: smolvm isn't installed here \(/no/smolvm\)"):
            runner.build(context, "localhost/bottle/x:latest", "abc")

    def test_a_runner_it_cant_reach_is_unreachable(self) -> None:
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "down"))
        with self.assertRaisesRegex(BottleError, r"runner box is unreachable \(ssh: connect to host down"):
            runner.state("bottle-b")

    def test_its_connection_is_shared_and_never_asks(self) -> None:
        command = ssh.SshTransport("box", "user@box.example").command(["smolvm", "machine", "exec"], tty=True)
        self.assertEqual(command[0], os.environ["BOTTLE_SSH"])
        for option in ("BatchMode=yes", "ControlMaster=auto", f"ControlPersist={ssh.PERSIST}"):
            self.assertIn(option, command)
        self.assertEqual(command[-4:], ["-t", "--", "user@box.example", "smolvm machine exec"])

    def test_a_shell_there_is_its_smolvm_exec_over_ssh(self) -> None:
        smolvm = self.has_smolvm()
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        command = runner.exec_command("bottle-b", ["bash", "-l"], user="genie", workdir="/workspace", tty=True)
        self.assertEqual(command[-3:-1], ["--", "box"])
        self.assertTrue(command[-1].startswith(f"{smolvm} --error-format json machine exec --name bottle-b -i -t"))
        result = subprocess.run(command, capture_output=True, text=True)   # runs there
        self.assertEqual(json.loads(result.stdout), {"state": "running"})

    def test_what_it_has(self) -> None:
        self.has_kvm()
        check = ssh.SshTransport("box", "box").check()
        self.assertEqual((check.os, check.kvm, check.git, check.smolvm), ("Linux", True, True, ""))
        self.assertEqual(check.missing(), [] if check.curl else ["curl, to install smolvm"])
        self.kvm.unlink()
        self.assertFalse(ssh.SshTransport("box", "box").check().kvm)

    def test_what_it_lacks_is_said_as_its_fixed(self) -> None:
        check = ssh.Check("Darwin", "arm64", False, "3.9", False, False, "")
        self.assertEqual(check.missing(), [
            "Linux (it runs Darwin)", "access to /dev/kvm (add the user to the kvm group)",
            "Python 3.11 or later as python3 (it has 3.9)", "git", "curl, to install smolvm",
        ])


class RunnerCommandsTest(SshTestCase):
    def test_add_checks_and_records_it_and_sends_its_code(self) -> None:
        self.has_kvm()
        self.has_smolvm()
        check = runners.add("box", "box")
        self.assertEqual(check.smolvm, "bottle")
        self.assertEqual(runners.registered(), {"box": {"ssh": "box"}})
        self.assertTrue((self.remotes / "box" / ".bottle" / "runner" / ssh.code_hash() / ".complete").exists())
        self.assertIsInstance(runners.get("box"), runners.RemoteRunner)

    def test_add_refuses_what_bottle_cant_set_up(self) -> None:
        with self.assertRaisesRegex(BottleError, r"box can't be a runner yet; it needs: access to /dev/kvm"):
            runners.add("box", "box")
        self.assertEqual(runners.registered(), {})

    def test_add_installs_smolvm_only_when_told_to(self) -> None:
        self.has_kvm()
        asked = []
        with self.assertRaisesRegex(BottleError, "box needs smolvm"):
            runners.add("box", "box", ask=lambda q: asked.append(q) or False)
        self.assertIn("Install its latest release into ~/.bottle/smolvm there?", asked[0])
        with mock.patch.object(ssh.SshTransport, "install_smolvm", side_effect=lambda: self.has_smolvm() and "1.33.0"):
            self.assertEqual(runners.add("box", "box", ask=lambda q: True).smolvm, "bottle")
        self.assertIn("box", runners.registered())

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
        self.has_smolvm()
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
