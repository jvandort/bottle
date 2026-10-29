import json
import shlex
import shutil
import stat
from pathlib import Path

from folio import broker, notes, repos, snapshots, store
from tests.support import FolioTestCase, git


class BrokerTest(FolioTestCase):
    def vault(self) -> Path:
        return broker.notes_path(broker.repo())

    def bottle(self) -> dict:
        return self.fake()["bottles"]["broker"]

    def test_the_broker_is_a_thread_that_mounts_only_its_notes(self) -> None:
        self.ok("broker")
        self.assertEqual(self.home / f"broker-{repos.path_hash(self.home / 'repos' / 'broker')}" / "broker" / "notes",
                         self.vault())
        self.assertEqual("main", git("-C", str(self.vault()), "symbolic-ref", "--short", "HEAD"))
        self.assertEqual(notes.GITIGNORE, (self.vault() / ".gitignore").read_text())
        self.assertEqual("", git("-C", str(self.vault()), "status", "--porcelain"))
        self.assertEqual([(notes.MOUNT, str(self.vault()))], [(m["path"], m["repo"]) for m in self.bottle()["mounts"]])
        self.assertFalse((self.home / "repos" / "broker").exists())
        self.assertEqual(self.calls("exec")[-1][-4:-2], ["attach", "claude"])

    def test_its_spec_is_folios_with_a_brokers_permissions(self) -> None:
        self.ok("broker")
        self.assertEqual({"features": ["claude", "tools"], "memory": "2G",
                          "permissions": ["list-bottles", "message-any"]}, self.bottle()["spec"])

    def test_the_vaults_main_tracks_its_lane_as_any_threads_notes(self) -> None:
        self.ok("broker")
        self.assertEqual(notes.REMOTES, self.bottle()["mounts"][0]["remotes"])
        # So bottle's follow moves it as the broker pushes main.
        self.assertEqual("refs/remotes/bottle/main",
                         git("-C", str(self.vault()), "for-each-ref", "--format=%(upstream)", "refs/heads/main"))

    def test_its_context_is_the_brokers(self) -> None:
        self.ok("broker")
        context = self.bottle()["context"]
        self.assertIn("You are the broker", context)
        self.assertIn("git push agent main", context)
        self.assertNotIn("genie/broker-1", context)  # a thread's branches aren't its memory
        self.assertIn("**`For you.md`**, at the root, is the user's", context)
        self.assertIn("**Your most important job is the user's priorities.**", context)
        self.assertIn("this overrides any default of committing", context)
        self.assertIn("## What survives a pause", context)

    def test_shell_and_claude_are_its_sessions(self) -> None:
        self.ok("broker", "shell")
        self.assertEqual(self.calls("exec")[-1][-4:-2], ["attach", "shell"])
        self.ok("broker", "claude")
        self.assertEqual(1, len(self.calls("new")))  # the same broker

    def test_pause_and_resume_keep_it_the_broker(self) -> None:
        self.ok("broker")
        self.ok("broker", "pause")
        self.assertNotIn("broker", self.fake()["bottles"])
        self.ok("broker")
        self.assertEqual(2, len(self.calls("new")))
        self.assertIn("message-any", self.bottle()["spec"]["permissions"])
        self.assertEqual("bottle", git("-C", str(self.vault()), "config", "branch.main.remote"))

    def test_pausing_no_broker(self) -> None:
        result = self.folio("broker", "pause")
        self.assertNotEqual(0, result.returncode)
        self.assertIn("no broker to pause", result.stderr)

    def test_a_repos_threads_never_get_its_permissions(self) -> None:
        self.ok("broker")
        self.ok("new", "fix-it")
        thread_bottle = next(b for name, b in self.fake()["bottles"].items() if name != "broker")
        self.assertNotIn("permissions", thread_bottle["spec"])

    def test_notes_say_where_the_vault_is_without_obsidian(self) -> None:
        self.env["FOLIO_OPEN"] = "no-such-opener"
        result = self.folio("broker", "notes")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn(str(self.vault()), result.stdout)
        self.assertNotIn("broker", self.fake()["bottles"])  # nothing started

    def obsidian(self, vaults: dict | None | str, running: bool = False) -> Path:
        """Fakes for `open` (Obsidian is installed), `pgrep` (it's running, or not) and `osascript` (quitting
        it works); each logs its arguments. Obsidian's list of vaults is `vaults`: None for none, a string for
        one that isn't JSON."""
        log = self.tmp / "commands"
        running_flag = self.tmp / "obsidian-running"
        if running:
            running_flag.touch()
        scripts = {
            "open": "",
            "pgrep": f"[ -e {shlex.quote(str(running_flag))} ]",
            "osascript": f"rm -f {shlex.quote(str(running_flag))}",
        }
        for name, body in scripts.items():
            script = self.tmp / name
            script.write_text(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> {shlex.quote(str(log))}\n{body}\n')
            script.chmod(script.stat().st_mode | stat.S_IEXEC)
            self.env[f"FOLIO_{name.upper()}"] = str(script)
        self.config = self.tmp / "obsidian" / "obsidian.json"
        if vaults is not None:
            self.config.parent.mkdir(exist_ok=True)
            self.config.write_text(vaults if isinstance(vaults, str) else json.dumps({"vaults": vaults, "theme": "x"}))
        self.env["FOLIO_OBSIDIAN_CONFIG"] = str(self.config)
        return log

    def opened(self, log: Path) -> list[str]:
        return [line.removeprefix("open ") for line in log.read_text().splitlines() if line.startswith("open obsidian:")]

    def test_the_vault_has_obsidians_folder_and_git_ignores_it(self) -> None:
        self.ok("broker", "notes")
        self.assertTrue((self.vault() / ".obsidian").is_dir())
        self.assertEqual("", git("-C", str(self.vault()), "status", "--porcelain", "--ignored=no"))

    def test_notes_open_a_vault_obsidian_knows_by_its_id(self) -> None:
        self.ok("broker", "notes")  # makes the vault
        log = self.obsidian({"0f0f0f0f0f0f0f0f": {"path": str(self.tmp), "ts": 1},  # a vault around it isn't it
                             "a1b2c3d4e5f6a7b8": {"path": str(self.vault()), "ts": 1}})
        before = self.config.read_text()
        out = self.ok("broker", "notes")
        self.assertEqual(["obsidian://open?vault=a1b2c3d4e5f6a7b8"], self.opened(log))
        self.assertEqual(before, self.config.read_text())
        self.assertIn("Opened", out)

    def test_notes_add_a_vault_obsidian_doesnt_know_and_open_it(self) -> None:
        for vaults in (None, {}, {"0f0f0f0f0f0f0f0f": {"path": "/elsewhere", "ts": 1}}):
            with self.subTest(vaults=vaults):
                log = self.obsidian(vaults)
                log.unlink(missing_ok=True)
                out = self.ok("broker", "notes")
                config = json.loads(self.config.read_text())
                [added] = [i for i, v in config["vaults"].items() if v["path"] == str(self.vault())]
                self.assertRegex(added, r"^[0-9a-f]{16}$")
                self.assertIsInstance(config["vaults"][added]["ts"], int)
                for other_id, other in (vaults or {}).items():
                    self.assertEqual(other, config["vaults"][other_id])
                if vaults is not None:
                    self.assertEqual("x", config["theme"])  # the rest of it is kept
                self.assertEqual([f"obsidian://open?vault={added}"], self.opened(log))
                self.assertIn("Opened", out)
                self.config.unlink()

    def test_a_running_obsidian_isnt_quit_without_asking(self) -> None:
        log = self.obsidian({}, running=True)
        out = self.ok("broker", "notes")  # no terminal to ask on
        self.assertIn("has to restart", out)
        self.assertEqual({"vaults": {}, "theme": "x"}, json.loads(self.config.read_text()))
        self.assertNotIn("osascript", log.read_text())
        self.assertEqual([], self.opened(log))

    def test_a_running_obsidian_is_quit_first_when_the_user_says_so(self) -> None:
        from unittest import mock
        from folio import obsidian
        log = self.obsidian({}, running=True)
        with mock.patch.dict("os.environ", self.env), mock.patch("sys.stdin.isatty", return_value=True), \
                mock.patch("builtins.input", return_value="y"):
            out = obsidian.open_vault(notes.ensure(self.vault()))
        commands = log.read_text().splitlines()
        quit_at = next(i for i, c in enumerate(commands) if c.startswith("osascript"))
        open_at = next(i for i, c in enumerate(commands) if c.startswith("open obsidian:"))
        self.assertLess(quit_at, open_at)
        self.assertEqual(1, len(json.loads(self.config.read_text())["vaults"]))
        self.assertIn("Opened", out)

    def test_an_obsidian_json_folio_doesnt_understand_is_left_alone(self) -> None:
        for config in ("not json", json.dumps({"vaults": []}), json.dumps(["x"])):
            with self.subTest(config=config):
                log = self.obsidian(config)
                log.unlink(missing_ok=True)
                out = self.ok("broker", "notes")
                self.assertEqual(config, self.config.read_text())
                self.assertEqual(["obsidian://choose-vault"], self.opened(log))
                self.assertIn("Open folder as vault", out)



class MigrationTest(FolioTestCase):
    """A broker from before notes: its vault at ~/.folio/repos/broker, its lane broker-out/-in, mounted at /workspace."""

    def setUp(self) -> None:
        super().setUp()
        self.old = self.home / "repos" / "broker"

    def make_old(self) -> None:
        self.old.mkdir(parents=True)
        git("init", "-q", "-b", "main", str(self.old))
        (self.old / "For you.md").write_text("priorities\n")
        git("-C", str(self.old), "add", ".")
        git("-C", str(self.old), "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "memory")
        self.head = git("-C", str(self.old), "rev-parse", "HEAD")
        git("-C", str(self.old), "update-ref", "refs/namespaces/broker-out/refs/heads/main", self.head)
        git("-C", str(self.old), "update-ref", "refs/namespaces/broker-in/refs/heads/idea", self.head)
        git("-C", str(self.old), "remote", "add", "bottle/broker", str(self.old))

    def new(self) -> Path:
        return broker.notes_path(broker.repo())

    def assert_moved(self) -> None:
        self.assertFalse(self.old.exists())
        self.assertEqual("priorities\n", (self.new() / "For you.md").read_text())
        self.assertEqual(self.head, git("-C", str(self.new()), "rev-parse", "refs/namespaces/notes-out/refs/heads/main"))
        self.assertEqual(self.head, git("-C", str(self.new()), "rev-parse", "refs/namespaces/notes-in/refs/heads/idea"))
        self.assertEqual("", git("-C", str(self.new()), "for-each-ref", "refs/namespaces/broker-out",
                                 "refs/namespaces/broker-in"))
        self.assertNotIn("bottle/broker", git("-C", str(self.new()), "remote").split())
        self.assertEqual("bottle", git("-C", str(self.new()), "config", "branch.main.remote"))

    def test_a_broker_with_no_bottle_moves_its_vault_history_lane_and_all(self) -> None:
        self.make_old()
        result = self.folio("broker")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("Moved the broker's notes", result.stderr)
        self.assert_moved()
        self.assertEqual(str(self.new()), self.fake()["bottles"]["broker"]["mounts"][0]["repo"])

    def test_a_running_brokers_vault_moves_when_its_paused(self) -> None:
        self.ok("broker")
        shutil.rmtree(self.new())   # as a broker started before notes has none
        self.make_old()
        self.env["FOLIO_OPEN"] = "no-such-opener"
        result = self.folio("broker", "notes")
        self.assertIn(str(self.old), result.stdout)
        self.assertIn("when it's next paused", result.stderr)
        self.assertTrue(self.old.exists())
        self.ok("broker", "pause")
        self.assert_moved()

    def test_a_snapshot_from_before_restores_its_vault_and_conversation_at_notes(self) -> None:
        self.ok("broker")
        home = Path(self.fake()["bottles"]["broker"]["home"])
        (home / ".claude" / "projects" / "-workspace").mkdir(parents=True)
        (home / ".claude" / "projects" / "-workspace" / "s.jsonl").write_text("talk\n")
        (Path(self.fake()["bottles"]["broker"]["mounts"][0]["dir"]) / "mine.md").write_text("unpushed\n")
        self.ok("broker", "pause")
        slot = store.current_slot(broker.repo().directory / broker.NAME)
        (slot / snapshots.NOTES_ARCHIVE).rename(slot / snapshots.WORKSPACE_ARCHIVE)
        self.ok("broker", "shell")
        home = Path(self.fake()["bottles"]["broker"]["home"])
        self.assertEqual("talk\n", (home / ".claude" / "projects" / "-notes" / "s.jsonl").read_text())
        self.assertFalse((home / ".claude" / "projects" / "-workspace").exists())
        mount = Path(self.fake()["bottles"]["broker"]["mounts"][0]["dir"])
        self.assertEqual("unpushed\n", (mount / "mine.md").read_text())

    def test_thread_commands_dont_work_in_the_old_vault(self) -> None:
        self.make_old()
        result = self.folio("claude", "other", cwd=self.old)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("folio broker", result.stderr)
