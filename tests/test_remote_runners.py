import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from unittest import mock

from bottle import prereqs, runners
from bottle.errors import BottleError
from tests.support import GitTestCase, built_cli

# ssh, as far as bottle-cli uses it: options up to --, the destination, and a command for its shell, run here as
# if at the destination, in a home of its own. "down" can't be reached. The destination is Linux, on what this
# machine is, and its shell sees $FAKE_KVM where it says /dev/kvm, so a test says whether it has KVM.
FAKE_SSH = r"""#!/usr/bin/env bash
while [ "$1" != "--" ]; do shift; done
shift
destination=$1; shift
echo "$destination $*" >> "$FAKE_SSH_LOG"
if [ "$destination" = down ]; then
  echo "ssh: connect to host down port 22: Connection refused" >&2
  exit 255
fi
script="$*"; script=${script//\/dev\/kvm/$FAKE_KVM}
home="$FAKE_REMOTES/$destination"; mkdir -p "$home"
cd "$home" && exec env -u BOTTLE_HOME HOME="$home" bash -c "$script"
"""

# bottle-cli there: every machine is running; it says where its home is; an image build says what it was sent.
FAKE_CLI = r"""#!/usr/bin/env bash
case "$1 $2" in
  "info --json") printf '{"memory_mib": 4096, "cpus": 32, "home": "%s/.bottle"}' "$HOME" ;;
  "image build") echo "built from $(cat "$(dirname "$3")/context/install.sh")" >&2; exit 1 ;;
  *) echo '{"state": "running"}' ;;
esac
"""


class RemoteRunnerTestCase(GitTestCase):
    """bottle, as a controller, and runner box: this machine, in a home of its own, reached by a stand-in ssh;
    this repo's bottle-cli here, and a stand-in for it there."""

    def setUp(self) -> None:
        super().setUp()
        cli = built_cli()
        short = Path(tempfile.mkdtemp(dir="/tmp"))
        self.addCleanup(shutil.rmtree, short, True)
        self.remotes, self.log, self.kvm = short / "remotes", short / "ssh.log", short / "kvm"
        (short / "ssh").write_text(FAKE_SSH)
        (short / "ssh").chmod(0o755)
        # What bottle-cli copies to a runner of this machine's architecture: the stand-in, built "from these
        # sources".
        arch = subprocess.run(["uname", "-m"], capture_output=True, text=True).stdout.strip()
        self.dist = short / "dists" / f"dist-{arch}-unknown-linux-musl"
        self.dist.mkdir(parents=True)
        (self.dist / "bottle-cli").write_text(FAKE_CLI)
        (self.dist / "bottle-cli").chmod(0o755)
        (self.dist / prereqs.STAMP).write_text("these sources")
        env = mock.patch.dict(os.environ, {
            "BOTTLE_CLI": str(cli), "BOTTLE_SSH": str(short / "ssh"), "BOTTLE_RUNNER_DISTS": str(short / "dists"),
            "FAKE_SSH_LOG": str(self.log), "FAKE_REMOTES": str(self.remotes), "FAKE_KVM": str(self.kvm),
        })
        env.start()
        self.addCleanup(env.stop)
        # Building bottle-cli for a runner is the dev shim's (prereqs.ensure_dist): built "already".
        self.built: list[str] = []
        patcher = mock.patch.object(prereqs, "ensure_dist", side_effect=lambda target: self.built.append(target))
        patcher.start()
        self.addCleanup(patcher.stop)
        patcher = mock.patch.object(prereqs, "ensure_cli")
        patcher.start()
        self.addCleanup(patcher.stop)

    def has_kvm(self) -> None:
        self.kvm.write_text("")

    def logged(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []


class AddTest(RemoteRunnerTestCase):
    def test_add_checks_it_and_bottle_cli_copies_itself_there(self) -> None:
        self.has_kvm()
        check = runners.add("box", "box")
        self.assertEqual((check.os, check.missing), ("Linux", ()))
        self.assertEqual(runners.registered(), {"box": {"ssh": "box"}})
        self.assertEqual((self.remotes / "box" / ".bottle" / "dist" / "bottle-cli").read_text(), FAKE_CLI)
        self.assertEqual(self.built, [self.dist.name.removeprefix("dist-")])
        self.assertIsInstance(runners.get("box"), runners.RemoteRunner)

    def test_add_refuses_what_bottle_cant_set_up(self) -> None:
        with self.assertRaisesRegex(BottleError, r"box can't be a runner yet; it needs: access to /dev/kvm"):
            runners.add("box", "box")
        self.assertEqual(runners.registered(), {})

    def test_unreachable_isnt_added(self) -> None:
        with self.assertRaisesRegex(BottleError, r"unreachable \(ssh: connect to host down"):
            runners.add("box", "down")
        self.assertEqual(runners.registered(), {})

    def test_status_says_what_each_has(self) -> None:
        self.has_kvm()
        runners.register("box", "box")
        runners.register("gone", "down")
        rows = {row["name"]: row for row in runners.status()}
        self.assertEqual((rows["box"]["reachable"], rows["box"]["os"], rows["box"]["missing"]), (True, "Linux", []))
        self.assertEqual(rows["gone"]["reachable"], False)
        self.assertIn("unreachable", rows["gone"]["error"])


class RemoteRunnerTest(RemoteRunnerTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.has_kvm()
        runners.add("box", "box")
        self.box = runners.get("box")

    def test_its_bottle_cli_runs_there_and_nothing_else_of_bottles(self) -> None:
        self.assertEqual(self.box.state("bottle-b"), "running")
        self.assertIn('"$HOME/.bottle/dist/bottle-cli" machine status --name bottle-b --json', self.logged()[-1])
        self.assertEqual((self.box.memory_mib(), self.box.cpus()), (4096, 32))
        self.assertEqual(self.box.cli.home(), str(self.remotes / "box" / ".bottle"))

    def test_a_build_sends_its_context_with_the_recipe_and_builds_there(self) -> None:
        context = self.tmp / "context"
        context.mkdir()
        (context / "install.sh").write_text("the context's install.sh\n")
        with mock.patch.object(runners.images.krun, "ca_certificate", return_value=None), \
                mock.patch("sys.stderr"), self.assertRaisesRegex(BottleError, "image build failed"):
            self.box.build(context, "localhost/bottle/x:latest", "abc", {"pull": "debian", "steps": []})

    def test_a_shell_there_is_its_bottle_cli_exec_by_runner_slash_name(self) -> None:
        command = self.box.exec_command("bottle-b", ["bash", "-l"], user="genie", workdir="/workspace", tty=True)
        self.assertEqual(command[1:6], ["machine", "exec", "--name", "box/bottle-b", "-t"])

    def test_a_repos_objects_are_pushed_into_a_mirror_there(self) -> None:
        repo = self.make_repo("app")
        objects = self.box.objects(runners.repos.Repo("app", repo))
        mirror = self.remotes / "box" / ".bottle" / "git" / f"{runners.mirror_key(repo / '.git')}.git"
        self.assertEqual(objects, mirror / "objects")
        main = subprocess.run(["git", "--git-dir", mirror, "rev-parse", "refs/heads/main"], capture_output=True,
                              text=True).stdout.strip()
        self.assertEqual(main, subprocess.run(["git", "-C", repo, "rev-parse", "main"], capture_output=True,
                                              text=True).stdout.strip())

    def test_its_bottle_clis_failures_are_its_own_not_unreachable(self) -> None:
        there = self.remotes / "box" / ".bottle" / "dist" / "bottle-cli"
        there.write_text('#!/bin/sh\necho "bottle-cli: no machine m" >&2\nexit 255\n')
        self.assertIsNone(self.box.state("bottle-b"))

    def test_one_it_cant_reach_is_unreachable(self) -> None:
        runners.forget("box")
        runners.register("box", "down")
        with self.assertRaisesRegex(BottleError, r"runner box is unreachable \(ssh: connect to host down"):
            runners.get("box").memory_mib()
