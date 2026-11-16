"""folio's own bottles, end to end through `folio`: no stand-in engine, but a stand-in bottle-cli
(tests/fake_bottle_cli.py), and folio serve for real. What a thread's bottle is made of, in what order, what's
rolled back when a step fails, how it's started again, listed and deleted, and the images it's built from."""

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import unittest

from tests.bottle.support import bridge, manifest
from tests import state as kept
from tests.support import FOLIO, FolioTestCase, git

FAKE_CLI = Path(__file__).resolve().parent / "fake_bottle_cli.py"


class MachineTestCase(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.cli_state = self.tmp / "fake-cli" / "state.json"
        self.cli_state.parent.mkdir()
        self.env.pop("FOLIO_BOTTLE")
        self.env |= {"BOTTLE_CLI": str(FAKE_CLI), "FAKE_CLI_STATE": str(self.cli_state),
                     "BOTTLE_HOME": str(self.tmp / "bottle-home")}
        self.key = f"project-{__import__('hashlib').sha256(str(self.repo).encode()).hexdigest()[:8]}"
        # The Keychain, as tests/fake_security keeps it: empty to begin with.
        bin_ = self.tmp / "bin"
        bin_.mkdir()
        (bin_ / "security").symlink_to(Path(__file__).resolve().parent / "fake_security")
        self.env |= {"PATH": f"{bin_}:{self.env['PATH']}", "FAKE_KEYCHAIN": str(self.tmp / "keychain")}
        self.addCleanup(lambda: self.folio("serve", "stop"))

    def cli(self) -> dict:
        if not self.cli_state.exists():
            return {"machines": {}, "images": [], "calls": [], "execs": [], "builds": []}
        return json.loads(self.cli_state.read_text())

    def machine(self, name: str = "t") -> dict:
        return self.cli()["machines"][f"{self.key}-{name}"]

    def record(self, name: str = "t") -> dict:
        path = self.home / self.key / name / "bottles" / f"{self.key}-{name}.json"
        return json.loads(path.read_text())

    def execs(self, name: str = "t") -> list[dict]:
        return [e for e in self.cli()["execs"] if e["machine"] == f"{self.key}-{name}"]

    def calls(self, command: str) -> list[list[str]]:
        return [c for c in self.cli()["calls"] if c[0] == command]

    def set_machine(self, name: str, **fields) -> None:
        state = self.cli()
        state["machines"][f"{self.key}-{name}"].update(fields)
        self.cli_state.write_text(json.dumps(state))


class CreateTest(MachineTestCase):
    def test_a_thread_gets_a_bottle_made_whole(self) -> None:
        self.ok("new", "t")
        head = git("-C", str(self.repo), "rev-parse", "HEAD")
        record = self.record()
        self.assertEqual((record["name"], record["status"], record["features"], record["runner"]),
                         (f"{self.key}-t", "ready", ["tools"], "local"))
        self.assertEqual([(m["path"], m["branch"], m["commit"]) for m in record["mounts"]][0], ("/workspace", "main", head))
        self.assertEqual([m["path"] for m in record["mounts"]], ["/workspace", "/notes"])
        self.assertEqual(self.machine()["state"], "running")
        self.assertIn("Stopped folio serve", self.ok("serve", "stop"))  # started, to answer it

    def test_its_steps_in_order_the_record_first(self) -> None:
        self.ok("new", "t")
        machine = f"{self.key}-t"
        order = [c[0] for c in self.cli()["calls"] if c[0] in ("create", "start") and c[1] == machine]
        self.assertEqual(order, ["create", "start"])
        self.assertEqual([e["step"] for e in self.execs() if e["step"]], ["contract", "hosts", "mkdir", "workspace"])
        self.assertEqual([(e["user"]) for e in self.execs() if e["step"] in ("hosts", "mkdir")], ["root", "root"])

    def test_its_machine_is_made_with_its_image_memory_objects_and_policy(self) -> None:
        self.ok("configure", "--memory", "3G", "--disk", "50G")
        self.ok("new", "t")
        machine = self.machine()
        self.assertEqual((machine["image"], machine["mem"], machine["storage"], machine["cpus"]),
                         ("localhost/bottle/base:with-tools", "3072", "50", str(os.cpu_count())))
        self.assertEqual((machine["user"], machine["workdir"], machine["labels"]), ("genie", "/workspace", {"thread": "t"}))
        self.assertEqual(machine["env"], {"BOTTLE_WORKDIR": "/workspace", "BOTTLE_MIRROR_ENV": "BOTTLE_WORKDIR"})
        self.assertIn(f"{self.repo}/.git/objects:/mnt/objects/workspace:ro", machine["mounts"])
        routes = machine["network"]["routes"]
        self.assertEqual(routes[0]["address"], "192.0.2.1")
        self.assertTrue(routes[0]["socket"].endswith("services/host.sock"), routes)
        self.assertIn("deb.debian.org", [c["host"] for c in machine["network"]["cache"]])
        self.assertEqual(self.record()["network_version"], 3)

    def test_its_workspace_borrows_the_repos_objects_and_serves_its_lane(self) -> None:
        self.ok("new", "t")
        [script] = [e for e in self.execs() if e["step"] == "workspace"]
        self.assertEqual(script["user"], "genie")
        text = script["argv"][-1]
        self.assertIn("echo /mnt/objects/workspace > /workspace/.git/objects/info/alternates", text)
        self.assertIn("git -C /workspace remote add origin http://bottle.host/git/workspace/origin", text)
        self.assertIn("git -C /workspace remote add agent http://bottle.host/git/workspace/agent", text)
        self.assertIn("git -C /workspace fetch -q --multiple origin host agent user", text)
        self.assertIn('> "$HOME/BOTTLE.md"', text)
        self.assertIn("running in bottle `", text)
        [hosts] = [e for e in self.execs() if e["step"] == "hosts"]
        self.assertIn("192.0.2.1 bottle.host", hosts["argv"][-1])

    def test_a_branch_with_odd_characters_is_quoted(self) -> None:
        git("-C", str(self.repo), "branch", "we$rd;branch")
        self.ok("new", "t", "--from", "we$rd;branch")
        [script] = [e for e in self.execs() if e["step"] == "workspace"]
        self.assertIn("git init -q -b 'we$rd;branch' /workspace", script["argv"][-1])

    def test_a_branch_it_doesnt_have_is_refused_before_anything_is_made(self) -> None:
        result = self.folio("new", "t", "--from", "nope")
        self.assertEqual(result.returncode, 1)
        self.assertIn("has no branch or commit 'nope'", result.stderr)
        self.assertEqual(self.calls("create"), [])

    def test_images_are_built_once_then_kept(self) -> None:
        self.ok("new", "t")
        self.assertEqual([b["tag"] for b in self.cli()["builds"]],
                         ["localhost/bottle/base:latest", "localhost/bottle/base:with-tools"])
        base = self.cli()["builds"][0]
        self.assertEqual(base["recipe"]["pull"], "debian:trixie-slim")
        self.assertIn("bottle.inputs", base["recipe"]["labels"])
        self.ok("new", "u")
        self.assertEqual(len(self.cli()["builds"]), 2)

    def test_an_image_gone_stale_is_removed_when_a_bottle_goes(self) -> None:
        self.ok("new", "t")
        state = self.cli()
        state["images"].append({"tag": "localhost/bottle/base:with-gone", "stage": "s", "disks": ["s.ext4"],
                                "labels": {"bottle.inputs": "old", "bottle.features": "gone"}})
        self.cli_state.write_text(json.dumps(state))
        self.ok("delete", "t", "--yes")
        self.assertNotIn("localhost/bottle/base:with-gone", [i["tag"] for i in self.cli()["images"]])
        self.assertIn("localhost/bottle/base:with-tools", [i["tag"] for i in self.cli()["images"]])


class RollbackTest(MachineTestCase):
    def test_a_failure_at_any_step_leaves_nothing(self) -> None:
        for step in ("create", "start", "contract", "hosts", "mkdir", "workspace"):
            with self.subTest(step):
                result = self.folio("new", "t", FAKE_CLI_FAIL=step)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(f"{step} failed", result.stderr)
                self.assertEqual(self.cli()["machines"], {})
                # No thread, and no lane; its notes are kept, as a deleted thread's are.
                self.assertFalse((self.home / self.key / "t" / "current").exists())
                self.assertFalse((self.home / self.key / "t" / "bottles").exists())
                self.assertEqual(git("-C", str(self.repo), "remote"), "")

    def test_a_failed_cleanup_keeps_a_broken_record(self) -> None:
        result = self.folio("new", "t", FAKE_CLI_FAIL="workspace,delete")
        self.assertEqual(result.returncode, 1)
        self.assertIn("workspace failed; cleanup also failed:", result.stderr)

    def test_one_that_doesnt_meet_the_contract_is_refused(self) -> None:
        result = self.folio("new", "t", FAKE_CLI_MISSING="git is installed")
        self.assertEqual(result.returncode, 1)
        self.assertIn("base+tools doesn't meet the bottle contract: git is installed", result.stderr)


class InterruptTest(MachineTestCase):
    """Ctrl-C is a failure like any other: what was being made is undone."""

    def interrupted(self, slow: str, again_at: str | None = None) -> tuple[int, str]:
        """`folio new t`, interrupted as a terminal does (Ctrl-C to folio and what it runs, its process group) while
        the stand-in is busy at `slow`; and again, once it's at `again_at`. Its exit status and stderr."""
        import signal
        import time

        folio = subprocess.Popen([str(FOLIO), "new", "t"], cwd=self.repo, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 env={**self.env, "FAKE_CLI_SLOW": slow, "FAKE_CLI_SLOW_SECONDS": "3"}, text=True,
                                 start_new_session=True)
        for step in (slow.split(",")[0], again_at):
            if step is None:
                continue
            deadline = time.monotonic() + 30
            while not self.calls(step) and time.monotonic() < deadline:
                time.sleep(0.05)
            time.sleep(0.3)   # the stand-in, asleep
            os.killpg(folio.pid, signal.SIGINT)
        _, err = folio.communicate(timeout=30)
        return folio.returncode, err

    def test_an_interrupted_bottle_is_rolled_back(self) -> None:
        returncode, err = self.interrupted("start")
        self.assertEqual(returncode, 130, err)
        self.assertIn("creating failed, rolling back: interrupted", err)
        self.assertTrue(err.endswith("folio: error: interrupted\n"), err)
        self.assertEqual(self.cli()["machines"], {})
        self.assertFalse((self.home / self.key / "t" / "bottles").exists())
        self.assertIsNone(kept.current_slot(self.home / self.key / "t"))
        self.assertEqual(git("-C", str(self.repo), "remote"), "")

    def test_its_rollback_isnt_interrupted(self) -> None:
        returncode, err = self.interrupted("start,delete", again_at="delete")
        self.assertEqual(returncode, 130, err)
        self.assertNotIn("cleanup also failed", err)
        self.assertEqual(self.cli()["machines"], {})
        self.assertFalse((self.home / self.key / "t" / "bottles").exists())


class LogTest(MachineTestCase):
    """A command's log, folio.log in its home's logs/: what it was asked, what it made and deleted, what it said, and
    how it ended."""

    def log(self) -> list[str]:
        """Its records, without when and which process."""
        return [line.split(" ", 3)[3] for line in (self.home / "logs" / "folio.log").read_text().splitlines()]

    def test_what_a_command_did_and_how_it_ended(self) -> None:
        self.ok("new", "t")
        self.ok("delete", "t", "--yes")
        log = self.log()
        self.assertEqual(log[0], "command folio new t")
        self.assertIn(f"bottles Creating bottle {self.key}-t...", log)
        self.assertTrue(any(line.startswith(f"bottles {self.key}-t: created on local, {self.repo} at main on /workspace")
                            for line in log), log)
        self.assertRegex(log[log.index("command folio delete t --yes") - 1], r"^command exited 0 in \d+\.\ds$")
        self.assertIn(f"bottles {self.key}-t: deleted", log)
        self.assertIn("threads t: deleted", log)
        mode = (self.home / "logs" / "folio.log").stat().st_mode & 0o777
        self.assertEqual(mode, 0o600)

    def test_a_failure_is_recorded_with_what_was_said(self) -> None:
        self.folio("new", "t", FAKE_CLI_FAIL="start")
        log = self.log()
        self.assertIn(f"bottles {self.key}-t: creating failed, rolling back: start failed", log)
        self.assertRegex(log[-1], r"^command failed in \d+\.\ds: start failed$")


class LifecycleTest(MachineTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "t")

    def test_a_stopped_bottle_is_started_again_bound(self) -> None:
        self.set_machine("t", state="stopped")
        self.ok("serve", "stop")
        result = self.folio("exec", "t", "--", "true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.machine()["state"], self.machine()["started"]), ("running", 2))
        self.assertNotIn("configured", self.machine())  # made with its policy
        self.assertIn("Stopped folio serve", self.ok("serve", "stop"))

    def test_one_made_with_an_older_policy_gets_this_one_and_is_checked_again(self) -> None:
        path = self.home / self.key / "t" / "bottles" / f"{self.key}-t.json"
        record = json.loads(path.read_text())
        path.write_text(json.dumps(record | {"network_version": 0}))
        self.set_machine("t", state="stopped")
        self.ok("exec", "t", "--", "true")
        self.assertEqual(self.machine()["configured"], 1)
        self.assertEqual(self.record()["network_version"], 3)
        self.assertEqual([e["step"] for e in self.execs() if e["step"]][-2:], ["contract", "hosts"])

    def test_a_command_runs_as_genie_at_its_first_mount_and_exits_as_it_does(self) -> None:
        result = self.folio("exec", "t", "--", "sh", "-c", "echo out; exit 3")
        self.assertEqual((result.returncode, result.stdout), (3, "out\n"))
        last = self.execs()[-1]
        self.assertEqual((last["user"], last["workdir"], last["argv"]), ("genie", "/workspace",
                                                                         ["sh", "-c", "echo out; exit 3"]))

    def test_listed_by_its_machines_state(self) -> None:
        self.ok("new", "u")
        self.set_machine("u", state="stopped")
        rows = {line.split()[0]: line.split()[1] for line in self.ok("list").splitlines()[1:]}
        self.assertEqual(rows, {"t": "running", "u": "stopped"})
        state = self.cli()
        del state["machines"][f"{self.key}-u"]
        self.cli_state.write_text(json.dumps(state))
        rows = {line.split()[0]: line.split()[1] for line in self.ok("list").splitlines()[1:]}
        self.assertEqual(rows["u"], "missing")

    def test_deleting_takes_every_part(self) -> None:
        self.ok("delete", "t", "--yes")
        self.assertEqual(self.cli()["machines"], {})
        self.assertFalse((self.home / self.key / "t" / "bottles").exists())

    def test_a_failed_delete_leaves_it_broken_and_another_finishes_it(self) -> None:
        result = self.folio("delete", "t", "--yes", FAKE_CLI_FAIL="delete")
        self.assertEqual(result.returncode, 1)
        self.assertIn("try again, and what's left is deleted", result.stderr)
        self.assertEqual(self.record()["status"], "broken")
        self.ok("delete", "t", "--yes")
        self.assertEqual(self.cli()["machines"], {})


class CredentialTest(MachineTestCase):
    def test_a_feature_with_a_credential_gets_its_stand_in_never_the_credential(self) -> None:
        self.ok("configure", "--feature", "github")
        result = self.folio("new", "t")
        self.assertEqual(result.returncode, 0, result.stderr)
        env = self.machine()["env"]
        standins = {k: v for k, v in env.items() if v == "the-egress-proxy-holds-the-real-one"}
        self.assertTrue(standins, env)
        self.assertEqual(env["BOTTLE_MIRROR_ENV"], " ".join(sorted(set(env) - {"BOTTLE_MIRROR_ENV"})))
        self.assertIn("isn't set: `folio auth set github`", result.stderr)
        hosts = [r.get("host") for r in self.machine()["network"]["routes"]]
        self.assertIn("api.github.com", hosts)


class AuthTest(MachineTestCase):
    """`folio auth`: credentials set once, in the Keychain; what bottles get is theirs to say."""

    def rows(self) -> dict[str, list[str]]:
        lines = self.ok("auth", "list").splitlines()
        self.assertEqual(lines[0].split(), ["CREDENTIAL", "FEATURE", "STATUS", "ATTACHED", "TO", "DESCRIPTION"])
        return {line.split()[0]: line for line in lines[1:]}

    def test_listed_with_where_each_goes_and_whether_its_set(self) -> None:
        rows = self.rows()
        self.assertEqual(sorted(rows), ["claude", "develocity", "github", "teamcity"])
        self.assertRegex(rows["claude"], r"^claude +claude +not set +api\.anthropic\.com +")
        self.assertIn("~/.gradle/develocity/keys.properties, else not set", rows["develocity"])

    def test_set_from_stdin_listed_then_removed(self) -> None:
        result = self.folio("auth", "set", "claude", input="tok-123\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "Stored claude. Bottles with it get it when they next start.\n")
        self.assertRegex(self.rows()["claude"], r"^claude +claude +set +")
        self.assertNotIn("tok-123", (self.tmp / "keychain" / "argv").read_text())  # never on a command line
        self.assertEqual(self.ok("auth", "rm", "claude"), "Removed claude\n")
        self.assertEqual(self.ok("auth", "remove", "claude"), "claude wasn't set\n")

    def test_only_a_credential_a_feature_declares(self) -> None:
        result = self.folio("auth", "set", "nope", input="x")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr, "folio: error: no feature declares a credential named 'nope' "
                                        "(known: claude, develocity, github, teamcity)\n")

    def test_a_value_it_cant_store_is_refused(self) -> None:
        result = self.folio("auth", "set", "claude", input='a"b')
        self.assertEqual(result.returncode, 1)
        self.assertIn("the value is empty or has characters folio can't store", result.stderr)

    def test_a_bottle_whose_credential_is_set_is_told_nothing_of_it(self) -> None:
        self.ok("configure", "--feature", "claude")
        self.folio("auth", "set", "claude", input="tok")
        result = self.folio("new", "t")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("isn't set", result.stderr)
        self.assertEqual(self.machine()["env"]["CLAUDE_CODE_OAUTH_TOKEN"], "the-egress-proxy-holds-the-real-one")


class RunnerTestCase(MachineTestCase):
    """Runner box: ssh to it a stand-in (tests/fake_ssh), running "there" here, in a home of its own, and the
    bottle-cli folio copies there the stand-in, with a state of its own."""

    def setUp(self) -> None:
        super().setUp()
        short = Path(tempfile.mkdtemp(dir="/tmp"))
        self.addCleanup(shutil.rmtree, short, True)
        self.remotes, self.there_state = short / "remotes", short / "there.json"
        (short / "ssh").symlink_to(Path(__file__).resolve().parent / "fake_ssh")
        (short / "kvm").write_text("")
        # The stand-in runner's, as Linux names it (tests/fake_ssh): a Mac's arm64 is aarch64.
        arch = subprocess.run(["uname", "-m"], capture_output=True, text=True).stdout.strip().replace("arm64", "aarch64")
        dist = short / "dists" / f"dist-{arch}-unknown-linux-musl"
        dist.mkdir(parents=True)
        (dist / "bottle-cli").write_text(f"#!/bin/sh\nFAKE_CLI_STATE={self.there_state} exec {sys.executable} {FAKE_CLI} \"$@\"\n")
        (dist / "bottle-cli").chmod(0o755)
        manifest(dist)
        self.env |= {"FOLIO_SSH": str(short / "ssh"), "FOLIO_RUNNER_DISTS": str(short / "dists"),
                     "FAKE_SSH_LOG": str(short / "ssh.log"), "FAKE_REMOTES": str(self.remotes),
                     "FAKE_KVM": str(short / "kvm"), "FAKE_SSH_OFF": str(short / "off")}
        self.off = short / "off"
        self.ok("runner", "add", "box", "--ssh", "box")

    def there(self) -> dict:
        return json.loads(self.there_state.read_text())

    def set_there(self, name: str, **fields) -> None:
        state = self.there()
        state["machines"][f"{self.key}-{name}"].update(fields)
        self.there_state.write_text(json.dumps(state))


class RemoteRunnerTest(RunnerTestCase):
    """A thread on a runner: its repo mirrored there, its images built there, its machine made there."""

    def test_a_thread_on_a_runner_is_made_there(self) -> None:
        result = self.folio("new", "t", "--runner", "box")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.cli()["machines"], {})   # nothing here
        machine = self.there()["machines"][f"{self.key}-t"]
        self.assertEqual(machine["state"], "running")
        self.assertEqual([b["tag"] for b in self.there()["builds"]],
                         ["localhost/bottle/base:latest", "localhost/bottle/base:with-tools"])
        # The repo's objects, from a mirror there brought up to date from here.
        key = f"project-{hashlib.sha256(str(self.repo / '.git').encode()).hexdigest()[:8]}"
        mirror = self.remotes / "box" / ".folio" / "git" / f"{key}.git"
        self.assertEqual(git("--git-dir", str(mirror), "rev-parse", "main"), git("-C", str(self.repo), "rev-parse", "main"))
        self.assertIn(f"{mirror}/objects:/mnt/objects/workspace:ro", machine["mounts"])
        # Its routes to folio serve's sockets there, which folio serve forwards here.
        socket = machine["network"]["routes"][0]["socket"]
        self.assertTrue(socket.startswith(f"{self.remotes / 'box'}/.folio/services/"), socket)
        self.assertEqual(self.record()["runner"], "box")
        rows = {line.split()[0]: line.split()[2] for line in self.ok("list").splitlines()[1:]}
        self.assertEqual(rows, {"t": "box"})

    def test_a_runner_with_bottles_isnt_removed(self) -> None:
        self.ok("new", "t", "--runner", "box")
        result = self.folio("runner", "rm", "box")
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"runner box has bottles: {self.key}-t; delete their threads first", result.stderr)
        self.assertEqual(json.loads(self.ok("runner", "ls", "--json"))[0]["name"], "box")
        self.ok("delete", "t", "--yes")
        self.assertEqual(self.there()["machines"], {})
        self.ok("runner", "rm", "box")


class WorkspaceTest(MachineTestCase, unittest.IsolatedAsyncioTestCase):
    """A thread's bottle's setup script, run for real (here, its paths its own) against folio serve answering its
    machine at bottle.host, as bottle-cli would carry its connections there."""

    async def asyncSetUp(self) -> None:
        self.ok("new", "t")
        [step] = [e for e in self.execs() if e["step"] == "workspace"]
        script = step["argv"][-1]
        self.workspace, self.notes, self.genie = self.tmp / "workspace", self.tmp / "notes-there", self.tmp / "genie"
        self.genie.mkdir()
        mounts = {m["path"]: m for m in self.record()["mounts"]}
        repos = {"/workspace": self.repo, "/notes": self.home / self.key / "t" / "notes"}
        # Each mount where it's a path here, not where it's in a remote's URL (bottle.host/git/workspace/...).
        objects = {f"\1{i}\1": (mount["objects"], str(repos[path] / ".git" / "objects"))
                   for i, (path, mount) in enumerate(mounts.items())}
        for placeholder, (there, _) in objects.items():
            script = script.replace(there, placeholder)
        for path, here in (("/workspace", self.workspace), ("/notes", self.notes)):
            script = script.replace(f"/git{path}/", "\0").replace(path, str(here)).replace("\0", f"/git{path}/")
        for placeholder, (_, here) in objects.items():
            script = script.replace(placeholder, here)
        server, port = await bridge(self.home / "services" / "host.sock", "192.0.2.1", 80, f"{self.key}-t", "bottle.host")
        self.addAsyncCleanup(self.close, server)
        self.git_env = {**self.env, "HOME": str(self.genie), "GIT_CONFIG_GLOBAL": str(self.genie / ".gitconfig"),
                        "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": f"url.http://127.0.0.1:{port}/.insteadOf",
                        "GIT_CONFIG_VALUE_0": "http://bottle.host/"}
        result = await self.run_(["sh", "-c", script])
        self.assertEqual(result.returncode, 0, result.stderr)

    @staticmethod
    async def close(server) -> None:
        server.close()
        await server.wait_closed()

    async def run_(self, argv: list[str]) -> subprocess.CompletedProcess:
        import asyncio
        return await asyncio.to_thread(subprocess.run, argv, cwd=self.tmp, env=self.git_env, capture_output=True,
                                       text=True)

    async def test_it_checks_out_the_branch_borrowing_the_repos_objects(self) -> None:
        head = git("-C", str(self.repo), "rev-parse", "HEAD")
        self.assertEqual(git("-C", str(self.workspace), "symbolic-ref", "--short", "HEAD"), "main")
        self.assertEqual(git("-C", str(self.workspace), "rev-parse", "HEAD"), head)
        self.assertEqual(git("-C", str(self.workspace), "count-objects"), "0 objects, 0 kilobytes")
        self.assertEqual(git("-C", str(self.workspace), "status", "--porcelain"), "")

    async def test_it_fetched_its_remotes(self) -> None:
        self.assertEqual(git("-C", str(self.workspace), "remote").split(), ["agent", "host", "origin", "user"])
        self.assertIn("host/main", git("-C", str(self.workspace), "branch", "-r", "--format=%(refname:short)").split())
        self.assertEqual(git("-C", str(self.notes), "remote").split(), ["agent", "host", "user"])

    async def test_it_writes_what_the_agent_is_told(self) -> None:
        told = (self.genie / "BOTTLE.md").read_text()
        self.assertIn(f"You're `genie`, running in bottle `{self.key}-t`", told)
        self.assertIn("# Thread `t`", told)

    async def test_a_push_to_agent_lands_in_the_threads_lane(self) -> None:
        self.commit(self.workspace, "agent work")
        commit = git("-C", str(self.workspace), "rev-parse", "HEAD")
        result = await self.run_(["git", "-C", str(self.workspace), "push", "-q", "agent", "main:genie/t-1"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(git("-C", str(self.repo), "rev-parse", "refs/namespaces/t-out/refs/heads/genie/t-1"), commit)
        git("-C", str(self.repo), "fetch", "-q", "bottle/t")
        self.assertEqual(git("-C", str(self.repo), "rev-parse", "bottle/t/genie/t-1"), commit)
