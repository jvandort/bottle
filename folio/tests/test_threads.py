import io
import json
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

from folio import cli, repos, threads
from tests.support import FolioTestCase, git


class NewTest(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.key = repos.current(self.repo).key

    def test_creates_the_bottle_and_lane_and_runs_nothing(self) -> None:
        out = self.ok("new", "fix-it")
        self.assertIn("Started thread fix-it", out)
        self.assertIn("Work in it with: folio claude fix-it", out)
        bottle = self.fake()["bottles"][f"{self.key}-fix-it"]
        self.assertEqual(bottle["git_repo"], str(self.repo))
        self.assertEqual(bottle["remotes"], ["origin:fetch=upstream", "host:fetch=heads",
                                             "agent:fetch=ns/fix-it-out,push=ns/fix-it-out", "user:fetch=ns/fix-it-in"])
        self.assertIsNone(bottle["checkout"])
        self.assertEqual(bottle["sessions"], {})
        self.assertEqual(self.calls("exec"), [])
        self.assertIn("# Thread `fix-it`", bottle["context"])
        self.assertIn("genie/fix-it-1", bottle["context"])
        self.assertIn("git fetch bottle/fix-it && git switch genie/<feature>-N", bottle["context"])
        self.assertIn("`git push -u agent <branch>`", bottle["context"])
        self.assertIn("**Fetch when you're given a prompt** (`git fetch user`)", bottle["context"])
        self.assertIn("its first branch should be\n`genie/fix-it-1`", bottle["context"])
        self.assertNotIn("work`", bottle["context"])
        self.assertEqual(git("-C", str(self.repo), "config", "remote.bottle/fix-it.uploadpack"),
                         "git --namespace=fix-it-out upload-pack")

    def test_from_a_branch(self) -> None:
        git("-C", str(self.repo), "branch", "release")
        self.ok("new", "fix-it", "--from", "release")
        self.assertEqual(self.fake()["bottles"][f"{self.key}-fix-it"]["checkout"], "release")

    def test_the_repos_spec_is_checked_and_saved_first(self) -> None:
        self.ok("new", "fix-it")
        self.assertEqual(repos.current(self.repo).spec(), {"features": ["tools"], "memory": "all"})
        self.assertEqual(self.fake()["bottles"][f"{self.key}-fix-it"]["spec"], {"features": ["tools"], "memory": "all"})

    def test_refuses_a_bad_name_or_a_taken_one(self) -> None:
        for name in ("Fix", "fix_it", "fix-", "fix--it", "fix/it"):
            with self.subTest(name):
                result = self.folio("new", name)
                self.assertEqual(result.returncode, 1)
                self.assertIn("invalid thread name", result.stderr)
        self.ok("new", "fix-it")
        result = self.folio("new", "fix-it")
        self.assertIn("thread fix-it already exists", result.stderr)

    def test_a_failed_bottle_leaves_nothing(self) -> None:
        self.env["FAKE_BOTTLE_FAIL_NEW"] = "1"
        result = self.folio("new", "fix-it")
        self.assertEqual(result.returncode, 1)
        self.assertIn("creating failed", result.stderr)
        self.assertEqual(git("-C", str(self.repo), "remote"), "")
        self.assertEqual(threads.all_(repos.current(self.repo)), [])

    def test_the_same_thread_name_in_two_repos_gets_two_bottles(self) -> None:
        other = self.make_repo("elsewhere/project")
        self.ok("new", "fix-it")
        self.ok("new", "fix-it", cwd=other)
        self.assertEqual(len(self.fake()["bottles"]), 2)


class AttachTest(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.bottle = f"{repos.current(self.repo).key}-fix-it"

    def set_bottle(self, **fields) -> None:
        state = self.fake()
        state["bottles"][self.bottle].update(fields)
        self.state.write_text(json.dumps(state))

    def test_claude_starts_the_session_once_and_attaches(self) -> None:
        self.ok("claude", "fix-it")
        self.assertEqual(self.fake()["bottles"][self.bottle]["sessions"], {"claude": threads.SESSIONS["claude"]})
        self.assertEqual(self.calls("exec")[-1], ["exec", self.bottle, "--", "tmux", "attach", "-d", "-t", "=claude"])
        self.ok("claude", "fix-it")
        sessions = [c for c in self.calls("exec") if c[4:6] == ["new-session", "-d"]]
        self.assertEqual(len(sessions), 1)  # reattached, not started again

    def test_a_shell_is_a_session_of_its_own(self) -> None:
        self.ok("shell", "fix-it")
        self.assertEqual(self.fake()["bottles"][self.bottle]["sessions"]["shell"], "exec bash -l")
        self.assertEqual(self.calls("exec")[-1][-1], "=shell")

    def test_a_stopped_bottle_is_started_and_claude_started_again(self) -> None:
        self.set_bottle(state="stopped", sessions={})  # the VM stopped, and its processes with it
        self.ok("claude", "fix-it")
        self.assertEqual(self.calls("start"), [["start", self.bottle]])
        self.assertEqual(self.fake()["bottles"][self.bottle]["sessions"], {"claude": threads.SESSIONS["claude"]})

    def test_a_missing_bottle_says_where_the_branches_are(self) -> None:
        state = self.fake()
        del state["bottles"][self.bottle]
        self.state.write_text(json.dumps(state))
        result = self.folio("claude", "fix-it")
        self.assertEqual(result.returncode, 1)
        self.assertIn("is gone, and with it everything that wasn't pushed", result.stderr)
        self.assertIn("What was pushed is in your repo: `git fetch bottle/fix-it`", result.stderr)
        self.assertIn("`folio delete fix-it` removes what's left of the thread", result.stderr)

    def test_an_unknown_thread_is_started_and_attached_to(self) -> None:
        for session in ("claude", "shell"):
            with self.subTest(session):
                name = f"new-{session}"
                self.assertIn(f"Started thread {name}", self.ok(session, name))
                bottle = f"{repos.current(self.repo).key}-{name}"
                self.assertEqual(self.calls("exec")[-1], ["exec", bottle, "--", "tmux", "attach", "-d", "-t", f"={session}"])
        self.assertIn("new-claude", git("-C", str(self.repo), "remote"))

    def test_an_unknown_thread_with_a_bad_name_starts_nothing(self) -> None:
        result = self.folio("claude", "Nope")
        self.assertEqual(result.returncode, 1)
        self.assertIn("invalid thread name 'Nope'", result.stderr)
        self.assertEqual(list(self.fake()["bottles"]), [self.bottle])

    def test_an_unknown_thread_elsewhere(self) -> None:
        result = self.folio("pause", "nope")
        self.assertIn("no thread named 'nope'", result.stderr)
        self.assertIn("(threads: fix-it)", result.stderr)

    def test_attaching_makes_it_most_recently_active(self) -> None:
        self.ok("new", "other")
        self.assertEqual([t.name for t in threads.all_(repos.current(self.repo))], ["other", "fix-it"])
        self.ok("claude", "fix-it")
        self.assertEqual([t.name for t in threads.all_(repos.current(self.repo))], ["fix-it", "other"])

    def test_list(self) -> None:
        git("-C", str(self.repo), "update-ref", "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1", "HEAD")
        lines = self.ok("list").splitlines()
        self.assertEqual(lines[0].split(), ["THREAD", "STATE", "SNAPSHOT", "ACTIVE", "FEATURES", "BRANCHES"])
        self.assertEqual(lines[1].split(), ["fix-it", "running", "-", "just", "now", "tools", "genie/fix-it-1"])


class DeleteTest(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.repo_ = repos.current(self.repo)
        self.bottle = f"{self.repo_.key}-fix-it"
        self.workspace = Path(self.fake()["bottles"][self.bottle]["workspace"])

    def delete_typing(self, typed: str | BaseException) -> tuple[int, str]:
        out = io.StringIO()
        answer = {"side_effect": typed} if isinstance(typed, BaseException) else {"return_value": typed}
        with mock.patch("sys.stdin.isatty", return_value=True), mock.patch("builtins.input", **answer), \
                redirect_stdout(out):
            code = cli.main(["delete", "fix-it"])
        return code, out.getvalue()

    def in_repo(self):
        return mock.patch("folio.repos.current", return_value=self.repo_)

    def test_yes_deletes_the_bottle_lane_remote_and_record(self) -> None:
        git("-C", str(self.repo), "update-ref", "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1", "HEAD")
        self.ok("delete", "fix-it", "--yes")
        self.assertEqual(self.fake()["bottles"], {})
        self.assertEqual(git("-C", str(self.repo), "for-each-ref", "refs/namespaces"), "")
        self.assertEqual(git("-C", str(self.repo), "remote"), "")
        self.assertFalse((self.repo_.directory / "fix-it").exists())

    def test_without_a_terminal_it_needs_yes(self) -> None:
        result = self.folio("delete", "fix-it")
        self.assertEqual(result.returncode, 1)
        self.assertIn("needs --yes", result.stderr)
        self.assertIn(self.bottle, self.fake()["bottles"])

    def test_says_what_would_be_lost_and_asks_for_the_name(self) -> None:
        head = git("-C", str(self.repo), "rev-parse", "HEAD")
        tree = git("-C", str(self.repo), "rev-parse", "HEAD^{tree}")
        pushed = git("-C", str(self.repo), "commit-tree", tree, "-p", head, "-m", "pushed")
        git("-C", str(self.repo), "update-ref", "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1", pushed)
        git("-C", str(self.workspace), "switch", "-q", "-c", "genie/fix-it-2")
        self.commit(self.workspace, "never pushed")
        (self.workspace / "notes.txt").write_text("draft")
        with self.in_repo():
            code, out = self.delete_typing("fix-it-not")
        self.assertEqual(code, 1)
        self.assertIn("genie/fix-it-1: pushed, but not on your branches or origin's", out)
        self.assertIn("genie/fix-it-2: in the bottle, never pushed", out)
        self.assertIn("uncommitted changes in the bottle", out)
        self.assertNotIn("main:", out)
        self.assertIn("Not deleted.", out)
        self.assertIn(self.bottle, self.fake()["bottles"])
        with self.in_repo():
            code, out = self.delete_typing("fix-it")
        self.assertEqual(code, 0)
        self.assertEqual(self.fake()["bottles"], {})

    def test_no_answer_deletes_nothing(self) -> None:
        for answer in (EOFError(), KeyboardInterrupt()):
            with self.subTest(answer), self.in_repo():
                code, out = self.delete_typing(answer)
            self.assertEqual(code, 1)
            self.assertIn("Not deleted.", out)
            self.assertIn(self.bottle, self.fake()["bottles"])

    def test_nothing_to_lose(self) -> None:
        with self.in_repo():
            code, out = self.delete_typing("fix-it")
        self.assertEqual(code, 0)
        self.assertIn("loses nothing", out)

    def test_a_stopped_bottle_is_started_to_look(self) -> None:
        state = self.fake()
        state["bottles"][self.bottle]["state"] = "stopped"
        self.state.write_text(json.dumps(state))
        with self.in_repo():
            self.delete_typing("fix-it")
        self.assertEqual(self.calls("start"), [["start", self.bottle]])

    def test_a_thread_whose_bottle_is_gone(self) -> None:
        state = self.fake()
        del state["bottles"][self.bottle]
        self.state.write_text(json.dumps(state))
        self.ok("delete", "fix-it", "--yes")
        self.assertEqual(self.calls("delete"), [])
        self.assertEqual(git("-C", str(self.repo), "remote"), "")


class ConfigureTest(FolioTestCase):
    def test_show_the_default_until_set(self) -> None:
        self.assertEqual(self.ok("configure", "show"), "features [tools], memory all (the default; nothing set yet)\n")

    def test_set_replaces_and_update_adds(self) -> None:
        self.assertEqual(self.ok("configure", "set", "--feature", "tools", "--feature", "jvm:version=17", "--memory", "8G"),
                         "Set project's spec: features [jvm:version=17, tools], memory 8G\n")
        self.ok("configure", "update", "--feature", "jvm:version=21", "--feature", "claude")
        self.assertEqual(repos.current(self.repo).spec(), {"features": ["claude", "jvm:version=21", "tools"], "memory": "8G"})
        self.ok("configure", "set", "--feature", "python")
        self.assertEqual(repos.current(self.repo).spec(), {"features": ["python"], "memory": "all"})

    def test_bottle_checks_it_before_its_saved(self) -> None:
        self.ok("configure", "set", "--feature", "tools")
        result = self.folio("configure", "update", "--feature", "nope")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr, "bottle: error: no feature named 'nope'\n"
                                        "folio: error: `bottle spec validate` failed (exit 1)\n")
        self.assertEqual(repos.current(self.repo).spec(), {"features": ["tools"], "memory": "all"})

    def test_update_needs_something_to_change(self) -> None:
        self.assertEqual(self.folio("configure", "update").returncode, 2)

    def test_a_new_thread_gets_the_spec(self) -> None:
        self.ok("configure", "set", "--feature", "tools", "--feature", "claude")
        self.ok("new", "fix-it")
        [bottle] = self.fake()["bottles"].values()
        self.assertEqual(bottle["spec"]["features"], ["claude", "tools"])


class RepoListTest(FolioTestCase):
    def test_every_repo_wherever_its_run(self) -> None:
        other = self.make_repo("other")
        self.ok("new", "a")
        self.ok("configure", "set", "--feature", "tools", cwd=other)
        lines = self.ok("repo", "list", cwd=self.tmp).splitlines()
        self.assertEqual(sorted(line.split()[1:] for line in lines[1:]), [[str(other), "0"], [str(self.repo), "1"]])
