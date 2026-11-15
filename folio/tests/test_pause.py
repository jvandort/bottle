import json
import subprocess
from pathlib import Path

from tests import state as kept
from tests.support import FolioTestCase, git


class PauseTestCase(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.repo_ = kept.repo(self.repo)
        self.bottle = f"{self.repo_.key}-fix-it"

    @property
    def workspace(self) -> Path:
        return Path(self.fake()["bottles"][self.bottle]["workspace"])

    @property
    def claude(self) -> Path:
        return Path(self.fake()["bottles"][self.bottle]["home"]) / ".claude"

    def ws(self, *args: str) -> str:
        return git("-C", str(self.workspace), *args)

    def slot(self) -> Path:
        return kept.current_slot(self.repo_.directory / "fix-it")

    def thread_state(self) -> str:
        return {line.split()[0]: line.split()[1] for line in self.ok("list").splitlines()[1:]}["fix-it"]


class PauseAndResumeTest(PauseTestCase):
    def make_a_mess(self) -> None:
        """Every kind of state a workspace has, at once."""
        (self.workspace / "tracked.txt").write_text("v1\n")
        (self.workspace / "gone.txt").write_text("doomed\n")
        (self.workspace / ".gitignore").write_text("build/\n")
        self.ws("add", ".")
        self.ws("switch", "-q", "-c", "genie/fix-it-1")
        self.ws("commit", "-q", "-m", "never pushed")
        (self.workspace / "tracked.txt").write_text("stashed\n")
        self.ws("stash", "-q")
        (self.workspace / "tracked.txt").write_text("staged\n")
        self.ws("add", "tracked.txt")
        (self.workspace / "tracked.txt").write_text("unstaged\n")
        (self.workspace / "gone.txt").unlink()
        (self.workspace / "untracked.txt").write_text("new\n")
        (self.workspace / "build").mkdir()
        (self.workspace / "build" / "out.bin").write_text("ignored")
        transcript = self.claude / "projects" / "-workspace"
        transcript.mkdir(parents=True)
        (transcript / "session.jsonl").write_text('{"the": "conversation"}\n')
        (transcript / "memory").mkdir()
        (transcript / "memory" / "fact.md").write_text("remembered")
        (self.claude / "plugins").mkdir()
        (self.claude / "plugins" / "cache.bin").write_text("regenerable")
        (self.claude / "settings.json").write_text('{"from": "the old bottle"}')

    def test_a_pause_and_resume_keep_everything_but_processes(self) -> None:
        self.make_a_mess()
        head = self.ws("rev-parse", "HEAD")
        self.ok("pause", "fix-it")
        self.assertNotIn(self.bottle, self.fake()["bottles"])
        self.assertEqual(self.thread_state(), "paused")
        self.assertEqual(self.ok("list").splitlines()[1].split()[:2], ["fix-it", "paused"])

        self.ok("claude", "fix-it")
        self.assertEqual(self.thread_state(), "running")
        self.assertEqual(self.ws("symbolic-ref", "--short", "HEAD"), "genie/fix-it-1")
        self.assertEqual(self.ws("rev-parse", "HEAD"), head)
        self.assertEqual(len(self.ws("stash", "list").splitlines()), 1)
        self.assertEqual(self.ws("show", ":tracked.txt"), "staged")
        self.assertEqual((self.workspace / "tracked.txt").read_text(), "unstaged\n")
        status = subprocess.run(["git", "-C", str(self.workspace), "status", "--porcelain"],
                                capture_output=True, text=True, check=True).stdout  # unstripped: its first column counts
        self.assertEqual(status.splitlines(), [" D gone.txt", "MM tracked.txt", "?? untracked.txt"])
        self.assertFalse((self.workspace / "build").exists())  # ignored: rebuilt, not carried
        self.assertEqual((self.claude / "projects" / "-workspace" / "session.jsonl").read_text(), '{"the": "conversation"}\n')
        self.assertEqual((self.claude / "projects" / "-workspace" / "memory" / "fact.md").read_text(), "remembered")
        self.assertFalse((self.claude / "plugins").exists())
        self.assertEqual((self.claude / "settings.json").read_text(), '{"from": "the old bottle"}')  # /model and all
        self.assertEqual(self.calls("exec")[-1][-4:-2], ["attach", "claude"])

    def test_a_resumed_thread_has_no_snapshot_left_to_go_stale(self) -> None:
        self.ok("pause", "fix-it")
        self.assertTrue((self.slot() / kept.WORKSPACE_ARCHIVE).exists())
        self.ok("shell", "fix-it")
        self.assertEqual(sorted(p.name for p in self.slot().iterdir()), ["thread.json"])
        self.assertEqual([p.name for p in self.slot().parent.iterdir() if p.name.startswith(".")], [])

    def test_the_sessions_are_stopped_before_the_snapshot(self) -> None:
        self.ok("claude", "fix-it")
        self.ok("pause", "fix-it")
        execs = self.calls("exec")
        kill = next(i for i, c in enumerate(execs) if c[3:5] == ["kill", "-HUP"])
        archive = next(i for i, c in enumerate(execs) if c[-1] == kept.workspace_out("/workspace"))
        self.assertLess(kill, archive)

    def test_a_resume_gets_the_repos_spec_as_it_is_now(self) -> None:
        self.ok("pause", "fix-it")
        self.ok("configure", "--feature", "claude")
        self.ok("claude", "fix-it")
        self.assertEqual(self.fake()["bottles"][self.bottle]["spec"]["features"], ["claude", "tools"])

    def test_a_resume_gets_the_advice_as_it_is_now(self) -> None:
        """Written afresh for the new bottle, not carried over from the old one."""
        self.ok("pause", "fix-it")
        # What it's told of the sidecar it's given meanwhile, say.
        self.ok("sidecar", "configure", "fix-it", "--runner", "box")
        self.ok("claude", "fix-it")
        self.assertIn("## Your sidecar", self.fake()["bottles"][self.bottle]["context"])

    def test_list_shows_what_a_paused_thread_takes_up(self) -> None:
        self.ok("new", "other")
        (self.workspace / "big.bin").write_bytes(b"x" * 300_000)  # untracked: in the snapshot
        self.ok("pause", "fix-it")
        size = sum((self.slot() / name).stat().st_size for name in kept.SNAPSHOT)
        rows = {line.split()[0]: line.split() for line in self.ok("list").splitlines()[1:]}
        self.assertEqual(rows["fix-it"][1:5], ["paused", "local", "-", f"{size / 1024:.1f}K"])
        self.assertEqual(rows["other"][1:5], ["running", "local", "-", "-"])

    def test_pausing_twice_or_with_no_bottle(self) -> None:
        self.ok("pause", "fix-it")
        self.assertIn("already paused", self.folio("pause", "fix-it").stderr)
        self.ok("new", "lost")
        state = self.fake()
        del state["bottles"][f"{self.repo_.key}-lost"]
        self.state.write_text(json.dumps(state))
        self.assertIn("there's nothing to pause", self.folio("pause", "lost").stderr)

    def test_a_stopped_bottle_is_started_to_pause_it(self) -> None:
        state = self.fake()
        state["bottles"][self.bottle]["state"] = "stopped"
        self.state.write_text(json.dumps(state))
        self.ok("pause", "fix-it")
        self.assertEqual(self.calls("start"), [["start", self.bottle]])

    def test_a_bottle_from_before_notes_pauses_without_them_and_resumes_with_them(self) -> None:
        state = self.fake()
        bottle = state["bottles"][self.bottle]
        bottle["mounts"] = [m for m in bottle["mounts"] if m["path"] != "/notes"]
        self.state.write_text(json.dumps(state))
        self.ok("pause", "fix-it")
        self.assertFalse((self.slot() / kept.NOTES_ARCHIVE).exists())
        self.ok("shell", "fix-it")
        self.assertIn("/notes", [m["path"] for m in self.fake()["bottles"][self.bottle]["mounts"]])


class FailureTest(PauseTestCase):
    def test_a_snapshot_that_doesnt_read_back_whole_deletes_nothing(self) -> None:
        self.env["FAKE_BOTTLE_GARBLE"] = "cd \"$HOME/.claude\" && tar -czf"
        result = self.folio("pause", "fix-it")
        self.assertEqual(result.returncode, 1)
        self.assertIn("claude.tar.gz came back incomplete", result.stderr)
        self.assertIn(self.bottle, self.fake()["bottles"])
        self.assertEqual(sorted(p.name for p in self.slot().iterdir()), ["thread.json"])
        self.assertEqual(self.thread_state(), "running")
        self.assertEqual([p.name for p in self.slot().parent.iterdir() if p.name.startswith(".staging")], [])

    def test_a_resume_that_fails_leaves_it_paused(self) -> None:
        self.ok("pause", "fix-it")
        result = self.folio("claude", "fix-it", FAKE_BOTTLE_FAIL_EXEC="find . -mindepth 1 -maxdepth 1 -exec rm -rf")
        self.assertEqual(result.returncode, 1)
        self.assertIn("failed", result.stderr)
        self.assertNotIn(self.bottle, self.fake()["bottles"])
        self.assertEqual(self.thread_state(), "paused")
        self.ok("claude", "fix-it")  # and it resumes once whatever it was is fixed
        self.assertEqual(self.thread_state(), "running")


class DeletePausedTest(PauseTestCase):
    def test_never_runs_git_on_the_snapshot(self) -> None:
        """A snapshot's .git is the agent's, and its config can make git run commands
        (core.fsmonitor runs on any git status): reading it here would run them outside the sandbox."""
        marker = self.tmp / "agent-ran-this"
        self.ws("config", "core.fsmonitor", f"touch {marker}; false")
        self.ok("pause", "fix-it")
        marker.unlink(missing_ok=True)  # the fake bottle runs on this machine; in a real one this was in the VM
        self.on_terminal(["delete", "fix-it"], [(b"Type fix-it to delete it: ", b"no\r")])
        self.assertFalse(marker.exists())


    def test_reads_what_would_be_lost_from_the_snapshot(self) -> None:
        self.ws("switch", "-q", "-c", "genie/fix-it-1")
        self.ws("commit", "-q", "--allow-empty", "-m", "never pushed")
        (self.workspace / "draft.txt").write_text("draft")
        self.ok("pause", "fix-it")
        code, out = self.on_terminal(["delete", "fix-it"], [(b"Type fix-it to delete it: ", b"fix-it\r")])
        self.assertEqual(code, 0, out)
        self.assertIn(b"genie/fix-it-1: in the snapshot, never pushed", out)
        self.assertIn(b"uncommitted changes in the snapshot", out)
        self.assertEqual(self.calls("delete"), [["delete", self.bottle]])  # the pause's; none for the delete
        self.assertEqual(["notes"], [p.name for p in (self.repo_.directory / "fix-it").iterdir()])
