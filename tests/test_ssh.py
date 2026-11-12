import json
import os
import subprocess
import sys
from pathlib import Path
from unittest import mock

from bottle import bottles, runners, ssh
from bottle.errors import BottleError
from tests.support import GitTestCase

# ssh, as far as bottle uses it: options up to --, the destination, and a command for its shell, run here
# as if at the destination, in a home of its own. "down" can't be reached. Whatever runs the tests, the
# destination is Linux on x86_64 (uname, from $FAKE_PATH), with no cargo or rustup on its PATH but the tests'
# own python3 (also from $FAKE_PATH: hiding a cargo can hide a python3 beside it), and what building
# bottle-cli takes besides (stand-ins, in $FAKE_PATH too). Its shell sees $FAKE_KVM where it says /dev/kvm, and
# $FAKE_HEADERS where it looks for the kernel's headers, so a test says whether the machine has them.
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
script=${script//\/usr\/include\/linux\/version.h/$FAKE_HEADERS}
mkdir -p "$FAKE_REMOTES/$destination"
path=$FAKE_PATH
IFS=: ; for dir in $PATH; do [ -x "$dir/cargo" ] || [ -x "$dir/rustup" ] || path="$path:$dir"; done; unset IFS
exec env -u BOTTLE_HOME HOME="$FAKE_REMOTES/$destination" PATH="$path" bash -c "$script"
"""

# rustup there, as `bottle runner add` installs it, with the musl target; and its cargo, whose `xtask dist
# --out DIR` makes a bottle-cli there.
FAKE_RUSTUP = """#!/bin/sh
echo x86_64-unknown-linux-musl
"""
FAKE_CARGO = """#!/bin/sh
echo "$*" >> "$HOME/cargo.log"
mkdir -p "$4"
cat > "$4/bottle-cli" <<'EOF'
#!/bin/sh
echo '{"state": "running"}'
EOF
chmod +x "$4/bottle-cli"
"""

# bottle-cli there: every machine is running.
FAKE_CLI = """#!/bin/sh
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
        for tool in ("cc", "make", "musl-gcc", "curl", "bsdtar"):
            (self.path / tool).write_text("#!/bin/sh\n")
            (self.path / tool).chmod(0o755)
        self.headers = self.tmp / "version.h"
        self.headers.write_text("")
        env = mock.patch.dict(os.environ, {
            "BOTTLE_SSH": str(fake), "FAKE_SSH_LOG": str(self.log), "FAKE_REMOTES": str(self.remotes),
            "FAKE_KVM": str(self.kvm), "FAKE_PATH": str(self.path), "FAKE_HEADERS": str(self.headers),
        })
        env.start()
        self.addCleanup(env.stop)

    def has_kvm(self) -> None:
        self.kvm.write_text("")

    def has_cli(self, destination: str = "box") -> Path:
        cli = self.remotes / destination / ".bottle" / "dist" / "bottle-cli"
        cli.parent.mkdir(parents=True)
        cli.write_text(FAKE_CLI)
        cli.chmod(0o755)
        return cli

    def has_rust(self, destination: str = "box") -> None:
        bin = self.remotes / destination / ".cargo" / "bin"
        bin.mkdir(parents=True)
        for name, script in (("rustup", FAKE_RUSTUP), ("cargo", FAKE_CARGO)):
            (bin / name).write_text(script)
            (bin / name).chmod(0o755)

    def logged(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []


class TransportTest(SshTestCase):
    def test_its_code_is_sent_the_first_time_and_run_there_after(self) -> None:
        self.has_cli()
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
        (context / "install.sh").write_text("#!/bin/sh\n")
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        # No bottle-cli there: the build fails there, before any VM, and says why, here.
        with mock.patch.object(ssh.SshTransport, "link"), mock.patch.dict(os.environ, {"BOTTLE_CLI": "/no/bottle-cli"}), \
                self.assertRaisesRegex(BottleError, r"runner box: .*/no/bottle-cli"):
            runner.build(context, "localhost/bottle/x:latest", "abc", {"pull": "debian", "steps": []})

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

    def test_a_shell_there_is_its_bottle_cli_exec_over_ssh(self) -> None:
        cli = self.has_cli()
        runner = runners.RemoteRunner("box", ssh.SshTransport("box", "box"))
        command = runner.exec_command("bottle-b", ["bash", "-l"], user="genie", workdir="/workspace", tty=True)
        self.assertEqual(command[-3:-1], ["--", "box"])
        self.assertTrue(command[-1].startswith(f"{cli} machine exec --name bottle-b -t"))
        result = subprocess.run(command, capture_output=True, text=True)   # runs there
        self.assertEqual(json.loads(result.stdout), {"state": "running"})

    def test_what_it_has(self) -> None:
        self.has_kvm()
        check = ssh.SshTransport("box", "box").check()
        self.assertEqual((check.os, check.kvm, check.git, check.rust, check.lacks), ("Linux", True, True, "", ()))
        self.assertEqual(check.missing(), [])  # Rust is bottle's to install
        self.has_rust()
        self.assertEqual(ssh.SshTransport("box", "box").check().rust, "ready")
        self.kvm.unlink()
        self.headers.unlink()
        check = ssh.SshTransport("box", "box").check()
        self.assertFalse(check.kvm)
        self.assertEqual(check.lacks, ("kernel-headers",))

    def test_no_route_on_a_mac_names_the_local_network_permission(self) -> None:
        why = "ssh: connect to host box.local port 22: No route to host"
        with mock.patch.object(ssh.platform, "system", return_value="Darwin"):
            self.assertIn("Local Network permission", ssh.explained(why))
            self.assertEqual(ssh.explained("ssh: Connection refused"), "ssh: Connection refused")
        with mock.patch.object(ssh.platform, "system", return_value="Linux"):
            self.assertEqual(ssh.explained(why), why)

    def test_what_it_lacks_is_said_as_its_fixed(self) -> None:
        check = ssh.Check("Darwin", "arm64", False, "3.9", False, "", ("musl-gcc", "curl"))
        self.assertEqual(check.missing(), [
            "Linux (it runs Darwin)", "access to /dev/kvm (add the user to the kvm group)",
            "Python 3.11 or later as python3 (it has 3.9)", "git",
            "musl-gcc, curl, to build bottle-cli (on Debian or Ubuntu: apt install build-essential musl-tools "
            "linux-libc-dev curl libarchive-tools)",
        ])


class RunnerCommandsTest(SshTestCase):
    def test_add_checks_and_records_it_and_sends_its_code(self) -> None:
        self.has_kvm()
        self.has_rust()
        check = runners.add("box", "box")
        self.assertEqual(check.rust, "ready")
        self.assertEqual(runners.registered(), {"box": {"ssh": "box"}})
        code = self.remotes / "box" / ".bottle" / "runner" / ssh.code_hash()
        self.assertTrue((code / ".complete").exists())
        self.assertTrue((code / "cli" / "src" / "main.rs").exists() and (code / "Cargo.lock").exists())
        self.assertIsInstance(runners.get("box"), runners.RemoteRunner)

    def test_add_builds_bottle_cli_there_once_for_its_sources(self) -> None:
        self.has_kvm()
        self.has_rust()
        runners.add("box", "box")
        home = self.remotes / "box"
        dist = home / ".bottle" / "dist"
        self.assertEqual((home / "cargo.log").read_text(), f"xtask dist --out {dist}\n")
        self.assertTrue((dist / "bottle-cli").exists())
        runner = runners.get("box")
        runner.ensure_ready()  # built from these already
        self.assertEqual(runner.state("bottle-b"), "running")
        self.assertEqual(len((home / "cargo.log").read_text().splitlines()), 1)

    def test_add_refuses_what_bottle_cant_set_up(self) -> None:
        with self.assertRaisesRegex(BottleError, r"box can't be a runner yet; it needs: access to /dev/kvm"):
            runners.add("box", "box")
        self.assertEqual(runners.registered(), {})

    def test_add_installs_rust_only_when_told_to(self) -> None:
        self.has_kvm()
        asked = []
        with self.assertRaisesRegex(BottleError, "box needs Rust, to build bottle-cli"):
            runners.add("box", "box", ask=lambda q: asked.append(q) or False)
        self.assertIn("Install rustup into ~/.cargo and ~/.rustup there?", asked[0])
        self.assertEqual(runners.registered(), {})
        with mock.patch.object(ssh.SshTransport, "install_rust", side_effect=self.has_rust):
            self.assertEqual(runners.add("box", "box", ask=lambda q: True).rust, "ready")
        self.assertIn("box", runners.registered())

    def test_a_failed_build_there_isnt_added(self) -> None:
        self.has_kvm()
        self.has_rust()
        (self.remotes / "box" / ".cargo" / "bin" / "cargo").write_text("#!/bin/sh\necho 'no linker' >&2\nexit 1\n")
        with self.assertRaisesRegex(BottleError, "runner box: `cargo xtask dist` failed"):
            runners.add("box", "box")
        self.assertEqual(runners.registered(), {})

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
        self.has_rust()
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
