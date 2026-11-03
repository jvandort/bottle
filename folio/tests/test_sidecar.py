import json
import subprocess
import sys

from folio import repos, threads
from tests.support import FOLIO, FolioTestCase


class SidecarTest(FolioTestCase):
    """A thread's sidecar: a bottle of its own, made and deleted with the thread's, which that one ssh-es to."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.repo_ = repos.current(self.repo)
        self.bottle = f"{self.repo_.key}-fix-it"
        self.sidecar = f"{self.bottle}-sidecar"

    def bottles(self) -> dict:
        return self.fake()["bottles"]

    def give_it_one(self) -> None:
        self.ok("sidecar", "set", "fix-it", "--runner", "desktop", "--feature", "jvm:version=21", "--memory", "32G")

    def test_set_says_it_comes_with_the_next_bottle(self) -> None:
        out = self.folio("sidecar", "set", "fix-it", "--runner", "desktop", "--memory", "32G").stdout
        self.assertIn("Set thread fix-it's sidecar: features [], memory 32G, runner desktop", out)
        self.assertIn("`folio pause fix-it`, then `folio claude fix-it`", out)
        self.assertNotIn(self.sidecar, self.bottles())
        self.assertEqual(threads.load(self.repo_, "fix-it").sidecar,
                         {"runner": "desktop", "spec": {"features": [], "memory": "32G"}})

    def test_a_resume_makes_it_first_and_the_threads_bottle_ssh_es_to_it(self) -> None:
        self.give_it_one()
        self.ok("pause", "fix-it")
        self.ok("claude", "fix-it")
        sidecar, bottle = self.bottles()[self.sidecar], self.bottles()[self.bottle]
        self.assertEqual((sidecar["runner"], sidecar["spec"]), ("desktop", {"features": ["jvm:version=21"], "memory": "32G"}))
        self.assertEqual(bottle["ssh"], {"sidecar": self.sidecar})
        made = [c[c.index("--name") + 1] for c in self.calls("new")]
        self.assertLess(made.index(self.sidecar), len(made) - 1)   # before the thread's last bottle
        self.assertEqual(made[-1], self.bottle)
        # The thread's repos and remotes, pushes included: its lane is the thread's.
        self.assertEqual([m["path"] for m in sidecar["mounts"]], ["/workspace", "/notes"])
        self.assertEqual([m["remotes"] for m in sidecar["mounts"]], [m["remotes"] for m in bottle["mounts"]])
        self.assertIn("agent:fetch=ns/fix-it-out,push=ns/fix-it-out", sidecar["mounts"][0]["remotes"])
        self.assertIn("a push from there lands in this thread's lane", bottle["context"])
        self.assertIn("`ssh sidecar` reaches this thread's sidecar", bottle["context"])
        self.assertIn("32G of memory and features `jvm:version=21`", bottle["context"])
        self.assertIn("This bottle is the sidecar of thread `fix-it`", sidecar["context"])

    def test_a_pause_deletes_it_and_a_delete_too(self) -> None:
        self.give_it_one()
        self.ok("pause", "fix-it")
        self.ok("claude", "fix-it")
        self.ok("pause", "fix-it")
        self.assertNotIn(self.sidecar, self.bottles())
        self.ok("claude", "fix-it")
        self.ok("delete", "fix-it", "--yes")
        self.assertEqual(self.bottles(), {})

    def test_it_starts_with_the_thread(self) -> None:
        self.give_it_one()
        self.ok("pause", "fix-it")
        self.ok("claude", "fix-it")
        state = self.fake()
        state["bottles"][self.sidecar]["state"] = "stopped"
        state["bottles"][self.bottle]["state"] = "stopped"
        self.state.write_text(json.dumps(state))
        self.ok("shell", "fix-it")
        self.assertEqual(self.bottles()[self.sidecar]["state"], "running")

    def test_update_show_and_remove(self) -> None:
        self.assertIn("no sidecar to update", self.folio("sidecar", "update", "fix-it", "--memory", "8G").stderr)
        self.give_it_one()
        self.ok("sidecar", "update", "fix-it", "--feature", "tools", "--memory", "64G", "--disk", "100G")
        self.assertEqual(self.ok("sidecar", "show", "fix-it").strip(),
                         f"features [jvm:version=21, tools], memory 64G, disk 100G, runner desktop; "
                         f"bottle {self.sidecar}, missing")
        self.assertEqual(self.ok("list").splitlines()[1].split()[3], "desktop")
        self.ok("sidecar", "remove", "fix-it")
        self.assertIsNone(threads.load(self.repo_, "fix-it").sidecar)
        self.assertEqual(self.ok("sidecar", "show", "fix-it").strip(), "Thread fix-it has no sidecar")

    def test_set_needs_a_runner_and_a_spec_bottle_takes(self) -> None:
        self.assertNotEqual(self.folio("sidecar", "set", "fix-it", "--memory", "8G").returncode, 0)
        result = self.folio("sidecar", "set", "fix-it", "--runner", "desktop", "--feature", "nope")
        self.assertIn("no feature named 'nope'", result.stderr)
        self.assertIsNone(threads.load(self.repo_, "fix-it").sidecar)

    def resume_failing(self, name: str) -> None:
        env = {**self.env, "FAKE_BOTTLE_FAIL_NEW": name}
        result = subprocess.run([sys.executable, str(FOLIO), "claude", "fix-it"], cwd=self.repo, env=env,
                                capture_output=True, text=True, input="")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(threads.states(self.repo_, [threads.load(self.repo_, "fix-it")])["fix-it"]["state"], "paused")
        self.assertEqual(self.bottles(), {})

    def test_a_restore_that_fails_leaves_it_paused_with_neither_bottle(self) -> None:
        from unittest import mock
        from folio import snapshots
        from folio.errors import FolioError

        self.give_it_one()
        self.ok("pause", "fix-it")
        with mock.patch.object(snapshots, "workspace_in", return_value="exit 3"), \
                mock.patch.object(threads.engine, "attach"), self.assertRaises(FolioError):
            threads.attach(self.repo_, threads.load(self.repo_, "fix-it"), threads.CLAUDE)
        self.assertEqual(self.bottles(), {})

    def test_a_resume_that_fails_leaves_it_paused_with_neither_bottle(self) -> None:
        self.give_it_one()
        self.ok("pause", "fix-it")
        self.resume_failing(self.sidecar)
        self.resume_failing(self.bottle)   # the sidecar made, then the thread's bottle not
        self.ok("claude", "fix-it")
        self.assertEqual(sorted(self.bottles()), [self.bottle, self.sidecar])

    def test_shell_only_while_the_thread_runs(self) -> None:
        self.assertIn("has no sidecar", self.folio("sidecar", "shell", "fix-it").stderr)
        self.give_it_one()
        self.assertIn("comes with its next bottle", self.folio("sidecar", "shell", "fix-it").stderr)
        self.ok("pause", "fix-it")
        self.assertIn("thread fix-it is paused", self.folio("sidecar", "shell", "fix-it").stderr)
        self.assertNotIn(self.sidecar, self.bottles())   # nothing made or started for it
        self.ok("claude", "fix-it")
        state = self.fake()
        state["bottles"][self.bottle]["state"] = "stopped"
        self.state.write_text(json.dumps(state))
        self.assertIn("thread fix-it is stopped", self.folio("sidecar", "shell", "fix-it").stderr)
        self.ok("shell", "fix-it")
        self.ok("sidecar", "shell", "fix-it")
        self.assertEqual(self.calls("exec")[-1], ["exec", self.sidecar, "--", "bash", "-l"])
