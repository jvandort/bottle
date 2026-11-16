import json
import shlex
from pathlib import Path

from tests import state as kept
from tests.support import FolioTestCase, git


class NewTest(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.key = kept.repo(self.repo).key

    def test_creates_the_bottle_and_lane_and_runs_nothing(self) -> None:
        out = self.ok("new", "fix-it")
        self.assertIn("Started thread fix-it", out)
        self.assertIn("Work in it with: folio claude fix-it", out)
        bottle = self.fake()["bottles"][f"{self.key}-fix-it"]
        [workspace, notes] = bottle["mounts"]
        self.assertEqual((notes["path"], notes["repo"]), ("/notes", str(kept.repo(self.repo).directory / "fix-it" / "notes")))
        self.assertEqual((workspace["path"], workspace["repo"]), ("/workspace", str(self.repo)))
        self.assertEqual(workspace["remotes"], ["origin:fetch=upstream", "host:fetch=heads",
                                                "agent:fetch=ns/fix-it-out,push=ns/fix-it-out", "user:fetch=ns/fix-it-in"])
        self.assertIsNone(workspace["checkout"])
        self.assertEqual(bottle["sessions"], {})
        self.assertEqual(self.calls("exec"), [])
        self.assertIn("# Thread `fix-it`", bottle["context"])
        self.assertIn("genie/fix-it-1", bottle["context"])
        self.assertIn("git fetch bottle/fix-it && git switch genie/<feature>-N", bottle["context"])
        self.assertIn("`git push -u agent <branch>`", bottle["context"])
        self.assertIn("**Fetch `user`** when you get a prompt", bottle["context"])
        self.assertIn("this thread's first is\n`genie/fix-it-1`", bottle["context"])
        self.assertIn("**Push as you go**, without being asked", bottle["context"])
        self.assertIn("`folio cp fix-it:<path> .`", bottle["context"])
        self.assertNotIn("work`", bottle["context"])
        self.assertEqual(git("-C", str(self.repo), "config", "remote.bottle/fix-it.uploadpack"),
                         "git --namespace=fix-it-out upload-pack")

    def test_from_a_branch(self) -> None:
        git("-C", str(self.repo), "branch", "release")
        self.ok("new", "fix-it", "--from", "release")
        self.assertEqual(self.fake()["bottles"][f"{self.key}-fix-it"]["mounts"][0]["checkout"], "release")

    def test_the_repos_spec_is_checked_and_saved_first(self) -> None:
        self.ok("new", "fix-it")
        self.assertEqual(kept.repo(self.repo).spec(), {"features": ["tools"], "memory": "2G"})
        self.assertEqual(self.fake()["bottles"][f"{self.key}-fix-it"]["spec"], {"features": ["tools"], "memory": "2G", "labels": {"thread": "fix-it"}})

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
        self.assertIsNone(kept.current_slot(kept.repo(self.repo).directory / "fix-it"))   # no thread

    def test_the_same_thread_name_in_two_repos_gets_two_bottles(self) -> None:
        other = self.make_repo("elsewhere/project")
        self.ok("new", "fix-it")
        self.ok("new", "fix-it", cwd=other)
        self.assertEqual(len(self.fake()["bottles"]), 2)


class AttachTest(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.bottle = f"{kept.repo(self.repo).key}-fix-it"

    def set_bottle(self, **fields) -> None:
        state = self.fake()
        state["bottles"][self.bottle].update(fields)
        self.state.write_text(json.dumps(state))

    def test_claude_starts_the_session_once_and_attaches(self) -> None:
        self.ok("claude", "fix-it")
        self.assertEqual(self.fake()["bottles"][self.bottle]["sessions"], {"claude": kept.SESSIONS["claude"]})
        self.assertEqual(self.calls("exec")[-1], ["exec", self.bottle, "--", "sh", "-c", kept.ATTACH, "attach", "claude", kept.session_command("claude"), "/workspace"])
        self.ok("claude", "fix-it")
        sessions = [c for c in self.calls("exec") if c[6:8] == ["attach", "--background"]]
        self.assertEqual(len(sessions), 1)  # reattached, not started again

    def test_the_attach_can_start_its_session_again(self) -> None:
        # With the command the session was started with, for a session shpool can no longer show.
        self.ok("claude", "fix-it")
        self.assertEqual(self.calls("exec")[-1][-2], shlex.join(["sh", "-c", kept.SESSIONS["claude"]]))
        started = next(c for c in self.calls("exec") if c[6:8] == ["attach", "--background"])
        self.assertEqual(started[-3:], ["--cmd", self.calls("exec")[-1][-2], "claude"])

    def test_a_shell_is_a_session_of_its_own(self) -> None:
        self.ok("shell", "fix-it")
        self.assertEqual(self.fake()["bottles"][self.bottle]["sessions"]["shell"], "exec bash -l")
        self.assertEqual(self.calls("exec")[-1][-3], "shell")

    def test_a_session_whose_program_exited_is_started_again(self) -> None:
        # shpool still lists it, and attaching would get a plain shell instead.
        self.set_bottle(sessions={"claude": kept.SESSIONS["claude"]}, exited=["claude"])
        self.ok("claude", "fix-it")
        self.assertIn(["exec", self.bottle, "--", "shpool", "kill", "claude"], self.calls("exec"))
        self.assertEqual(len([c for c in self.calls("exec") if c[6:8] == ["attach", "--background"]]), 1)
        self.assertEqual(self.fake()["bottles"][self.bottle].get("exited"), [])

    def test_a_stopped_bottle_is_started_and_claude_started_again(self) -> None:
        self.set_bottle(state="stopped", sessions={})  # the VM stopped, and its processes with it
        self.ok("claude", "fix-it")
        self.assertEqual(self.calls("start"), [["start", self.bottle]])
        self.assertEqual(self.fake()["bottles"][self.bottle]["sessions"], {"claude": kept.SESSIONS["claude"]})

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
                bottle = f"{kept.repo(self.repo).key}-{name}"
                self.assertEqual(self.calls("exec")[-1], ["exec", bottle, "--", "sh", "-c", kept.ATTACH, "attach", session, kept.session_command(session), "/workspace"])
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

    def test_attaching_makes_it_most_recently_active_and_leaves_its_configuration(self) -> None:
        self.ok("new", "other")
        repo = kept.repo(self.repo)
        recent = lambda: [line.split()[0] for line in self.ok("list").splitlines()[1:]]
        self.assertEqual(recent(), ["other", "fix-it"])
        slot = kept.current_slot(repo.directory / "fix-it")
        self.ok("claude", "fix-it")
        self.assertEqual(recent(), ["fix-it", "other"])
        self.assertEqual(kept.current_slot(repo.directory / "fix-it"), slot)   # thread.json not written

    def test_list(self) -> None:
        git("-C", str(self.repo), "update-ref", "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1", "HEAD")
        lines = self.ok("list").splitlines()
        self.assertEqual(lines[0].split(), ["THREAD", "STATE", "RUNNER", "SIDECAR", "SNAPSHOT", "ACTIVE", "FEATURES"])
        # Its branches aren't listed: a thread can have a great many.
        self.assertEqual(lines[1].split(), ["fix-it", "running", "local", "-", "-", "just", "now", "tools"])
        self.assertEqual(self.ok("ls"), self.ok("list"))
        self.assertEqual(self.ok("status"), self.ok("list"))


class DeleteTest(FolioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.repo_ = kept.repo(self.repo)
        self.bottle = f"{self.repo_.key}-fix-it"
        self.workspace = Path(self.fake()["bottles"][self.bottle]["workspace"])

    def delete_typing(self, typed: bytes) -> tuple[int, str]:
        """`folio delete fix-it` on a terminal, `typed` typed when it asks for the name."""
        code, out = self.on_terminal(["delete", "fix-it"], [(b"Type fix-it to delete it: ", typed)])
        return code, out.decode().replace("\r\n", "\n")

    def test_yes_deletes_the_bottle_lane_remote_and_record(self) -> None:
        git("-C", str(self.repo), "update-ref", "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1", "HEAD")
        self.ok("delete", "fix-it", "--yes")
        self.assertEqual(self.fake()["bottles"], {})
        self.assertEqual(git("-C", str(self.repo), "for-each-ref", "refs/namespaces"), "")
        self.assertEqual(git("-C", str(self.repo), "remote"), "")
        # The record and snapshot are gone; the notes stay, for a thread of the same name.
        self.assertEqual(["notes"], [p.name for p in (self.repo_.directory / "fix-it").iterdir()])

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
        code, out = self.delete_typing(b"fix-it-not\r")
        self.assertEqual(code, 1)
        self.assertIn("genie/fix-it-1: pushed, but not on your branches or origin's", out)
        self.assertIn("genie/fix-it-2: in the bottle, never pushed", out)
        self.assertIn("uncommitted changes in the bottle", out)
        self.assertNotIn("main:", out)
        self.assertIn("Not deleted.", out)
        self.assertIn(self.bottle, self.fake()["bottles"])
        code, out = self.delete_typing(b"fix-it\r")
        self.assertEqual(code, 0)
        self.assertEqual(self.fake()["bottles"], {})

    def test_no_answer_deletes_nothing(self) -> None:
        for answer in (b"\x04", b"\x03"):   # the end of input; Ctrl-C
            with self.subTest(answer):
                code, out = self.delete_typing(answer)
            self.assertEqual(code, 1)
            self.assertIn("Not deleted.", out)
            self.assertIn(self.bottle, self.fake()["bottles"])

    def test_nothing_to_lose(self) -> None:
        code, out = self.delete_typing(b"fix-it\r")
        self.assertEqual(code, 0)
        self.assertIn("loses nothing", out)

    def test_a_stopped_bottle_is_started_to_look(self) -> None:
        state = self.fake()
        state["bottles"][self.bottle]["state"] = "stopped"
        self.state.write_text(json.dumps(state))
        self.delete_typing(b"fix-it\r")
        self.assertEqual(self.calls("start"), [["start", self.bottle]])

    def test_a_bottle_that_cant_be_asked_is_said_to_lose_whatever_is_in_it(self) -> None:
        # One broken from inside (its /etc/passwd gone, say), so nothing runs in it.
        self.env["FAKE_BOTTLE_FAIL_EXEC"] = "for-each-ref"
        code, out = self.delete_typing(b"fix-it\r")
        self.assertEqual(code, 0, out)
        self.assertIn("whatever is in the bottle, which couldn't be asked what it has", out)
        self.assertEqual(self.fake()["bottles"], {})

    def test_a_thread_whose_bottle_is_gone(self) -> None:
        state = self.fake()
        del state["bottles"][self.bottle]
        self.state.write_text(json.dumps(state))
        self.ok("delete", "fix-it", "--yes")
        self.assertEqual(self.calls("delete"), [])
        self.assertEqual(git("-C", str(self.repo), "remote"), "")


class ConfigureTest(FolioTestCase):
    def test_without_options_it_shows_the_default_until_set(self) -> None:
        self.assertEqual(self.ok("configure"), "features [tools], memory 2G (the default; nothing set yet)\n")

    def test_features_added_given_options_and_taken_away_memory_and_disk_set(self) -> None:
        self.assertEqual(self.ok("configure", "--feature", "jvm:version=17", "--memory", "8G"),
                         "Set project's spec: features [jvm:version=17, tools], memory 8G\n")
        self.ok("configure", "--feature", "jvm:version=21", "--feature", "claude")
        self.assertEqual(kept.repo(self.repo).spec(), {"features": ["claude", "jvm:version=21", "tools"], "memory": "8G"})
        self.ok("configure", "--no-feature", "jvm", "--feature", "python")
        self.assertEqual(kept.repo(self.repo).spec(), {"features": ["claude", "python", "tools"], "memory": "8G"})
        self.assertEqual(self.ok("configure", "--disk", "50G"),
                         "Set project's spec: features [claude, python, tools], memory 8G, disk 50G\n")
        self.assertEqual(self.ok("configure"), "features [claude, python, tools], memory 8G, disk 50G\n")
        self.ok("configure", "--no-disk")
        self.assertNotIn("disk", kept.repo(self.repo).spec())

    def test_a_feature_taken_away_is_named_alone_and_must_be_there(self) -> None:
        for args, error in ((("--no-feature", "tools:x=1"), "takes a feature's name alone, e.g. tools"),
                            (("--no-feature", "tools", "--feature", "tools"), "both name tools"),
                            # A usage error names what's wrong, in the argument parser's own words.
                            (("--disk", "50G", "--no-disk"), "--no-disk"),
                            # A thread comes first: a word after a flag that takes none isn't one.
                            (("--no-disk", "50G"), "unexpected argument '50G': the thread comes first"),
                            (("--memory", "8G", "fix-it"), "unexpected argument 'fix-it'")):
            with self.subTest(args):
                result = self.folio("configure", *args)
                self.assertEqual(result.returncode, 2)
                self.assertIn(error, result.stderr)
        result = self.folio("sidecar", "configure", "--memory", "8G", "fix-it")
        self.assertEqual(result.returncode, 2)
        self.assertIn("the thread comes first, `folio sidecar configure THREAD [OPTIONS]`", result.stderr)
        result = self.folio("configure", "--no-feature", "jvm")
        self.assertEqual(result.returncode, 1)
        self.assertIn("there's no feature jvm to take away (features: tools)", result.stderr)
        self.assertIsNone(kept.repo(self.repo).spec())

    def test_bottle_checks_it_before_its_saved(self) -> None:
        self.ok("configure", "--memory", "2G")
        result = self.folio("configure", "--feature", "nope")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr, "bottle: error: no feature named 'nope'\n"
                                        "folio: error: `bottle spec validate` failed (exit 1)\n")
        self.assertEqual(kept.repo(self.repo).spec(), {"features": ["tools"], "memory": "2G"})

    def test_what_folio_needs_cant_be_taken_away_and_a_spec_saved_without_it_has_it(self) -> None:
        result = self.folio("configure", "--no-feature", "tools")
        self.assertEqual(result.returncode, 1)
        self.assertIn("folio's threads need the tools feature: shpool, which every thread's sessions run in",
                      result.stderr)
        self.assertIsNone(kept.repo(self.repo).spec())
        kept.repo(self.repo).save_spec({"features": ["claude"], "memory": "4G"})
        self.assertEqual(kept.repo(self.repo).spec(), {"features": ["claude", "tools"], "memory": "4G"})
        self.ok("new", "fix-it")
        [bottle] = self.fake()["bottles"].values()
        self.assertEqual(bottle["spec"]["features"], ["claude", "tools"])

    def test_a_new_thread_gets_the_spec_and_its_name(self) -> None:
        self.ok("configure", "--feature", "claude")
        self.ok("new", "fix-it")
        [bottle] = self.fake()["bottles"].values()
        self.assertEqual(bottle["spec"], {"features": ["claude", "tools"], "memory": "2G", "labels": {"thread": "fix-it"}})
        self.assertNotIn("labels", kept.repo(self.repo).spec())

    def test_a_threads_context_says_what_a_pause_keeps(self) -> None:
        self.ok("new", "fix-it")
        [bottle] = self.fake()["bottles"].values()
        self.assertIn("## What survives", bottle["context"])
        self.assertIn("A pause keeps `/workspace`, `/notes` and your conversation", bottle["context"])
        self.assertIn("## Your notes", bottle["context"])
        self.assertIn("git -C /notes push agent main", bottle["context"])
        self.assertIn("could resume this thread from them alone", bottle["context"])

    def test_a_repos_spec_cant_give_permissions_or_labels(self) -> None:
        for key, value in (("permissions", ["message-any"]), ("labels", {"thread": "other"})):
            with self.subTest(key):
                kept.repo(self.repo).save_spec({"features": ["tools"], "memory": "2G", key: value})
                for args in (("new", f"t-{key}"), ("configure", "--memory", "4G")):
                    result = self.folio(*args)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(f"can't give {key}", result.stderr)
        self.assertEqual(self.fake()["bottles"], {})


class RepoListTest(FolioTestCase):
    def test_every_repo_wherever_its_run(self) -> None:
        other = self.make_repo("other")
        self.ok("new", "a")
        self.ok("configure", "--memory", "2G", cwd=other)
        lines = self.ok("repo", "list", cwd=self.tmp).splitlines()
        self.assertEqual(sorted(line.split()[1:] for line in lines[1:]), [[str(other), "0"], [str(self.repo), "1"]])


class ThreadConfigureTest(FolioTestCase):
    """`folio configure THREAD`: what one thread has that the repo's settings don't, kept as a sidecar's are, over
    the repo's as they are when its next bottle's made."""

    def setUp(self) -> None:
        super().setUp()
        self.key = kept.repo(self.repo).key
        self.ok("new", "fix-it")

    def bottle(self, thread: str = "fix-it") -> dict:
        return self.fake()["bottles"][f"{self.key}-{thread}"]

    def settings(self) -> dict:
        return kept.thread(kept.repo(self.repo), "fix-it")["settings"]

    def test_without_options_it_shows_what_its_next_bottle_gets_and_whats_its_own(self) -> None:
        self.assertEqual(self.ok("configure", "fix-it"),
                         "features [tools], memory 2G, runner this machine; thread fix-it, running\n")
        self.ok("configure", "--runner", "desktop")
        self.assertIn("runner desktop;", self.ok("configure", "fix-it"))

    def test_one_threads_memory_and_features_its_own_and_the_repos_changes_still_reach_it(self) -> None:
        self.ok("new", "other")
        self.ok("pause", "fix-it")
        self.assertEqual(self.ok("configure", "fix-it", "--memory", "8G", "--feature", "jvm:version=21"),
                         "Set thread fix-it: features [tools, jvm:version=21 (overridden)], memory 8G (overridden), "
                         "runner this machine\n")
        self.assertEqual(self.settings(), {"features": ["jvm:version=21"], "memory": "8G"})
        self.ok("claude", "fix-it")
        self.assertEqual((self.bottle()["spec"]["memory"], self.bottle()["spec"]["features"]),
                         ("8G", ["jvm:version=21", "tools"]))
        self.assertEqual(self.bottle("other")["spec"]["memory"], "2G")
        # The repo's changes reach it, but for what's its own: its jvm's options are its own.
        self.ok("configure", "--feature", "python", "--feature", "jvm:version=17", "--disk", "50G")
        self.assertEqual(self.ok("configure", "fix-it").split(";")[0],
                         "features [jvm:version=21 (overridden), python, tools], memory 8G (overridden), disk 50G, "
                         "runner this machine")

    def test_no_takes_its_own_away_and_it_has_the_repos_again(self) -> None:
        self.ok("configure", "--feature", "jvm:version=17", "--disk", "50G")
        self.ok("configure", "fix-it", "--feature", "jvm:version=21", "--feature", "python", "--memory", "8G",
                "--disk", "100G", "--runner", "local")
        self.ok("configure", "fix-it", "--no-feature", "jvm", "--no-memory", "--no-disk", "--no-runner")
        self.assertEqual(self.settings(), {"features": ["python"]})
        self.assertEqual(self.ok("configure", "fix-it").split(";")[0],
                         "features [jvm:version=17, tools, python (overridden)], memory 2G, disk 50G, runner this machine")

    def test_a_feature_the_repo_gives_cant_be_taken_away(self) -> None:
        result = self.folio("configure", "fix-it", "--no-feature", "tools")
        self.assertEqual(result.returncode, 1)
        self.assertIn("thread fix-it doesn't override feature tools (overridden: none); "
                      "one the repo gives can't be taken away", result.stderr)

    def test_a_change_to_a_thread_with_a_bottle_comes_with_its_next(self) -> None:
        self.assertIn("It comes with the thread's next bottle: `folio pause fix-it`, then `folio claude fix-it`",
                      self.ok("configure", "fix-it", "--memory", "8G"))
        self.assertEqual(self.bottle()["spec"]["memory"], "2G")
        self.assertEqual(len(self.calls("new")), 1)   # nothing made

    def test_its_spec_is_checked_over_the_repos(self) -> None:
        result = self.folio("configure", "fix-it", "--feature", "nope")
        self.assertEqual(result.returncode, 1)
        self.assertIn("no feature named 'nope'", result.stderr)
        self.assertEqual(self.settings(), {})

    def test_a_paused_threads_runner_is_set_without_starting_it(self) -> None:
        self.ok("pause", "fix-it")
        self.assertEqual(self.ok("configure", "fix-it", "--runner", "desktop"),
                         "Set thread fix-it: features [tools], memory 2G, runner desktop (overridden)\n")
        self.assertNotIn(f"{self.key}-fix-it", self.fake()["bottles"])
        self.assertEqual(self.ok("list").splitlines()[1].split()[:3], ["fix-it", "paused", "desktop"])
        self.ok("claude", "fix-it")
        self.assertEqual(self.bottle()["runner"], "desktop")
        # `local`: this machine, whatever the repo's; --no-runner: the repo's again.
        self.ok("pause", "fix-it")
        self.ok("configure", "--runner", "desktop")
        self.ok("configure", "fix-it", "--runner", "local")
        self.assertIn("runner this machine (overridden);", self.ok("configure", "fix-it"))
        self.ok("configure", "fix-it", "--no-runner")
        self.assertIn("runner desktop;", self.ok("configure", "fix-it"))

    def test_a_thread_with_a_bottle_elsewhere_is_paused_to_move(self) -> None:
        result = self.folio("configure", "fix-it", "--runner", "desktop")
        self.assertEqual(result.returncode, 1)
        self.assertIn("thread fix-it's bottle is on local; pause it first (`folio pause fix-it`)", result.stderr)
        self.assertEqual(self.settings(), {})
        # Having the repo's again, which is elsewhere, is a move too.
        self.ok("configure", "fix-it", "--runner", "local")
        self.ok("configure", "--runner", "desktop")
        self.assertIn("pause it first", self.folio("configure", "fix-it", "--no-runner").stderr)


class RunnerTest(FolioTestCase):
    """Where a thread's bottles are made: the repo's runner, else the thread's own, applied as each is made."""

    def setUp(self) -> None:
        super().setUp()
        self.key = kept.repo(self.repo).key

    def runner(self, thread: str) -> str:
        return self.fake()["bottles"][f"{self.key}-{thread}"]["runner"]

    def test_a_thread_is_made_here_by_default(self) -> None:
        self.ok("new", "fix-it")
        self.assertEqual(self.runner("fix-it"), "local")
        self.assertNotIn("--runner", self.calls("new")[0])

    def test_a_thread_can_be_made_on_a_runner(self) -> None:
        self.ok("new", "fix-it", "--runner", "desktop")
        self.assertEqual(self.runner("fix-it"), "desktop")
        self.assertIn("desktop", self.ok("list").splitlines()[1].split())

    def test_the_repos_runner_is_its_threads_unless_one_has_its_own(self) -> None:
        self.assertEqual(self.ok("configure", "--runner", "desktop"), f"Set {kept.repo(self.repo).name}'s runner: desktop\n")
        self.assertIn("runner desktop", self.ok("configure"))
        self.ok("new", "one")
        self.ok("new", "two", "--runner", "local")
        self.assertEqual((self.runner("one"), self.runner("two")), ("desktop", "local"))

    def test_no_runner_is_this_machine_again(self) -> None:
        self.ok("configure", "--runner", "desktop")
        self.assertEqual(self.ok("configure", "--no-runner"), f"Set {kept.repo(self.repo).name}'s runner: this machine\n")
        self.assertNotIn("runner", self.ok("configure"))
        self.assertEqual(self.folio("configure", "--runner", "desktop", "--no-runner").returncode, 2)

    def test_a_paused_thread_resumes_on_the_runner_its_given(self) -> None:
        self.ok("new", "fix-it")
        self.ok("pause", "fix-it")
        self.ok("claude", "fix-it", "--runner", "desktop")
        self.assertEqual(self.runner("fix-it"), "desktop")
        # And stays there: its next bottle is made there too.
        self.ok("pause", "fix-it")
        self.ok("shell", "fix-it")
        self.assertEqual(self.runner("fix-it"), "desktop")

    def test_a_thread_with_a_bottle_elsewhere_is_paused_first(self) -> None:
        self.ok("new", "fix-it")
        result = self.folio("claude", "fix-it", "--runner", "desktop")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("thread fix-it's bottle is on local; pause it first (`folio pause fix-it`)", result.stderr)
        self.assertEqual(self.runner("fix-it"), "local")
