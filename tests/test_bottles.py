import unittest
from pathlib import Path
from unittest import mock

from bottle import bottles, repos
from bottle.errors import BottleError
from tests.support import GitTestCase, run


class DefaultNameTest(unittest.TestCase):
    def test_first_bottle_is_the_repo_name(self) -> None:
        self.assertEqual(bottles.default_name("gradle", set()), "gradle")

    def test_then_numbered_from_two(self) -> None:
        self.assertEqual(bottles.default_name("gradle", {"gradle"}), "gradle-2")
        self.assertEqual(bottles.default_name("gradle", {"gradle", "gradle-2"}), "gradle-3")

    def test_reuses_gaps(self) -> None:
        self.assertEqual(bottles.default_name("gradle", {"gradle", "gradle-3"}), "gradle-2")
        self.assertEqual(bottles.default_name("gradle", {"gradle-2"}), "gradle")


class FakeRuntime:
    """Stands in for runtime and daemon, recording what exists and what was done."""

    def __init__(self, fail: str | None = None, fail_cleanup: str | None = None) -> None:
        self.fail = fail
        self.fail_cleanup = fail_cleanup
        self.networks: set[str] = set()
        self.containers: dict[str, str] = {}
        self.egress: set[str] = set()
        self.calls: list[str] = []
        self.statuses_seen: list[str] = []

    def _step(self, name: str) -> None:
        self.calls.append(name)
        if self.fail == name:
            raise BottleError(f"{name} failed")

    def network_create(self, network):
        self.statuses_seen.append(bottles.load()[next(iter(bottles.load()))].status)
        self._step("network_create")
        self.networks.add(network)

    def network_gateway(self, network):
        return "192.168.128.1"

    def network_delete(self, network):
        if self.fail_cleanup == "network":
            raise BottleError("network busy")
        self.networks.discard(network)

    def container_run(self, name, image, network, env, mounts):
        self._step("container_run")
        self.run_args = dict(name=name, image=image, network=network, env=env, mounts=mounts)
        self.containers[name] = "running"

    def container_state(self, name):
        return self.containers.get(name)

    def container_start(self, name):
        self._step("container_start")
        self.containers[name] = "running"

    def container_stop(self, name):
        self.calls.append("container_stop")
        self.containers[name] = "stopped"

    def stop_daemon(self):
        self.calls.append("daemon_stop")
        return True

    def container_delete(self, name):
        self.containers.pop(name, None)

    def container_exec(self, name, argv, user=None, workdir=None):
        self._step("init_workspace")
        self.workspace_script = argv[-1]
        return ""

    def ensure_egress(self, bottle, network):
        self._step("ensure_egress")
        self.egress.add(bottle)
        return "http://192.168.128.1:3128"

    def release_egress(self, bottle):
        self.egress.discard(bottle)

    def patch(self, test: unittest.TestCase) -> None:
        for module, names in (
            (bottles.runtime, ("network_create", "network_gateway", "network_delete", "container_run",
                               "container_state", "container_start", "container_stop", "container_delete",
                               "container_exec")),
            (bottles.daemon, ("ensure_egress", "release_egress", "stop")),
        ):
            for name in names:
                patcher = mock.patch.object(module, name, getattr(self, "stop_daemon" if name == "stop" else name))
                patcher.start()
                test.addCleanup(patcher.stop)
        for patcher in (
            mock.patch.object(bottles.prereqs, "ensure_container"),
            mock.patch.object(bottles.runtime, "image_exists", return_value=True),
        ):
            patcher.start()
            test.addCleanup(patcher.stop)


class BottleTestCase(GitTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.repo_path = self.make_repo("gradle")
        self.head = run("git", "-C", self.repo_path, "rev-parse", "HEAD")
        repos.wrap(self.repo_path)

    def fake(self, **kwargs) -> FakeRuntime:
        fake = FakeRuntime(**kwargs)
        fake.patch(self)
        return fake

    def bottle_refs(self) -> str:
        return run("git", "-C", self.repo_path, "for-each-ref", "refs/bottle/")


class CreateTest(BottleTestCase):
    def test_creates_every_part(self) -> None:
        fake = self.fake()

        bottle = bottles.create("gradle", "tools")

        self.assertEqual((bottle.name, bottle.branch, bottle.commit, bottle.status), ("gradle", "main", self.head, "ready"))
        self.assertEqual(bottles.get("gradle"), bottle)
        self.assertIn(f"{self.head} commit\trefs/bottle/{bottle.id}", self.bottle_refs())
        self.assertEqual(fake.networks, {bottle.network})
        self.assertEqual(fake.containers, {bottle.container: "running"})
        self.assertEqual(fake.egress, {"gradle"})

    def test_order_container_before_egress(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "tools")
        self.assertEqual(fake.calls, ["network_create", "container_run", "ensure_egress", "init_workspace"])

    def test_record_is_written_before_anything_is_created(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "tools")
        self.assertEqual(fake.statuses_seen, ["creating"])

    def test_container_gets_image_proxy_and_read_only_objects(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "tools")
        self.assertEqual(fake.run_args["image"], "bottle/tools:latest")
        self.assertEqual(fake.run_args["env"]["HTTPS_PROXY"], "http://192.168.128.1:3128")
        self.assertEqual(fake.run_args["env"]["http_proxy"], "http://192.168.128.1:3128")
        [mount] = fake.run_args["mounts"]
        self.assertEqual((mount.source, mount.target, mount.readonly), (self.repo_path / ".git/objects", "/mnt/repo/objects", True))

    def test_workspace_script_quotes_values(self) -> None:
        run("git", "-C", self.repo_path, "branch", "we$rd;branch")
        fake = self.fake()
        bottles.create("gradle", "tools", branch="we$rd;branch")
        self.assertIn("git init -q -b 'we$rd;branch' /workspace", fake.workspace_script)

    def test_names_further_bottles(self) -> None:
        self.fake()
        names = [bottles.create("gradle", "tools").name for _ in range(3)]
        self.assertEqual(names, ["gradle", "gradle-2", "gradle-3"])

    def test_detached_start(self) -> None:
        self.commit(self.repo_path, "later")
        run("git", "-C", self.repo_path, "checkout", "-q", "--detach", self.head)
        fake = self.fake()
        bottle = bottles.create("gradle", "tools")
        self.assertEqual((bottle.branch, bottle.commit, bottle.checkout), (None, self.head, f"({self.head[:12]})"))
        self.assertIn(f"update-ref --no-deref HEAD {self.head}", fake.workspace_script)
        self.assertEqual(bottles.get("gradle"), bottle)  # survives the JSON round trip

    def test_explicit_name_and_branch(self) -> None:
        run("git", "-C", self.repo_path, "branch", "feature")
        self.fake()
        bottle = bottles.create("gradle", "tools", branch="feature", name="mine")
        self.assertEqual((bottle.name, bottle.branch), ("mine", "feature"))

    def test_duplicate_name(self) -> None:
        self.fake()
        bottles.create("gradle", "tools", name="mine")
        with self.assertRaisesRegex(BottleError, "already exists"):
            bottles.create("gradle", "tools", name="mine")

    def test_unknown_image(self) -> None:
        self.fake()
        with self.assertRaisesRegex(BottleError, "no image named 'nope'"):
            bottles.create("gradle", "nope")
        self.assertEqual(bottles.load(), {})

    def test_builds_the_image_first(self) -> None:
        fake = self.fake()
        with mock.patch.object(bottles.images, "ensure_built", side_effect=lambda i: fake.calls.append(f"ensure_built {i}")):
            bottles.create("gradle", "tools")
        self.assertEqual(fake.calls[0], "ensure_built tools")

    def test_unknown_repo(self) -> None:
        with self.assertRaisesRegex(BottleError, "no wrapped repo named 'nope'"):
            bottles.create("nope", "tools")

    def test_unknown_branch(self) -> None:
        self.fake()
        with self.assertRaisesRegex(BottleError, "has no branch 'nope'"):
            bottles.create("gradle", "tools", branch="nope")


class RollbackTest(BottleTestCase):
    def test_failure_at_each_step_leaves_nothing(self) -> None:
        for step in ("network_create", "container_run", "ensure_egress", "init_workspace"):
            with self.subTest(step):
                fake = self.fake(fail=step)
                with self.assertRaisesRegex(BottleError, f"{step} failed"):
                    bottles.create("gradle", "tools")
                self.assertEqual(bottles.load(), {})
                self.assertEqual(self.bottle_refs(), "")
                self.assertEqual((fake.networks, fake.containers, fake.egress), (set(), {}, set()))

    def test_interrupt_rolls_back(self) -> None:
        fake = self.fake()
        with mock.patch.object(bottles.runtime, "container_run", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                bottles.create("gradle", "tools")
        self.assertEqual(bottles.load(), {})
        self.assertEqual(fake.networks, set())

    def test_failed_cleanup_keeps_a_broken_record(self) -> None:
        self.fake(fail="init_workspace", fail_cleanup="network")
        with self.assertRaisesRegex(BottleError, "init_workspace failed; cleanup also failed: .*network busy"):
            bottles.create("gradle", "tools")
        self.assertEqual(bottles.get("gradle").status, "broken")


class DeleteTest(BottleTestCase):
    def test_removes_every_part(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "tools")
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})
        self.assertEqual(self.bottle_refs(), "")
        self.assertEqual((fake.networks, fake.containers, fake.egress), (set(), {}, set()))

    def test_finishes_off_a_half_made_bottle(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "tools")
        # As if creation died after the network: no container, no egress.
        fake.containers.clear()
        fake.egress.clear()
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})
        self.assertNotIn(bottle.network, fake.networks)

    def test_retry_after_a_failed_delete(self) -> None:
        fake = self.fake(fail_cleanup="network")
        bottles.create("gradle", "tools")
        with self.assertRaisesRegex(BottleError, "rerun `bottle delete gradle`"):
            bottles.delete("gradle")
        self.assertEqual(bottles.get("gradle").status, "broken")
        self.assertEqual(fake.containers, {})  # later steps still ran

        fake.fail_cleanup = None
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})

    def test_unknown_bottle(self) -> None:
        with self.assertRaisesRegex(BottleError, "no bottle named 'nope'"):
            bottles.delete("nope")


class EnsureRunningTest(BottleTestCase):
    def test_starts_a_stopped_bottle_and_its_egress(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "tools")
        fake.containers[bottle.container] = "stopped"
        fake.egress.clear()

        bottles.ensure_running("gradle")

        self.assertEqual(fake.containers[bottle.container], "running")
        self.assertEqual(fake.egress, {"gradle"})

    def test_running_bottle_only_ensures_egress(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "tools")
        fake.calls.clear()
        bottles.ensure_running("gradle")
        self.assertEqual(fake.calls, ["ensure_egress"])

    def test_missing_container(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "tools")
        fake.containers.clear()
        with self.assertRaisesRegex(BottleError, "container is gone; remove it with `bottle delete gradle`"):
            bottles.ensure_running("gradle")

    def test_unfinished_bottle(self) -> None:
        self.fake(fail="init_workspace", fail_cleanup="network")
        with self.assertRaises(BottleError):
            bottles.create("gradle", "tools")
        with self.assertRaisesRegex(BottleError, "gradle is broken"):
            bottles.ensure_running("gradle")


class StartStopTest(BottleTestCase):
    def test_stop_stops_the_vm_and_egress(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "tools")
        bottles.stop("gradle")
        self.assertEqual(fake.containers[bottle.container], "stopped")
        self.assertEqual(fake.egress, set())
        self.assertEqual(bottles.get("gradle").status, "ready")  # kept, just stopped

    def test_stop_a_stopped_bottle(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "tools")
        bottles.stop("gradle")
        fake.calls.clear()
        bottles.stop("gradle")
        self.assertEqual(fake.calls, [])

    def test_start_after_stop(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "tools")
        bottles.stop("gradle")
        bottles.start("gradle")
        self.assertEqual(fake.containers[bottle.container], "running")
        self.assertEqual(fake.egress, {"gradle"})

    def test_shutdown_stops_running_bottles_then_bottled(self) -> None:
        fake = self.fake()
        a = bottles.create("gradle", "tools")
        b = bottles.create("gradle", "tools")
        bottles.stop(b.name)
        fake.calls.clear()

        stopped, daemon_was_running = bottles.shutdown()

        self.assertEqual((stopped, daemon_was_running), (["gradle"], True))
        self.assertEqual(fake.containers, {a.container: "stopped", b.container: "stopped"})
        self.assertEqual(fake.calls, ["container_stop", "daemon_stop"])


class WorkspaceTestCase(BottleTestCase):
    """Runs the real git fetch, with a local repo standing in for the bottle's /workspace."""

    def setUp(self) -> None:
        super().setUp()
        self.fake()
        self.bottle = bottles.create("gradle", "tools")
        self.workspace = self.tmp / "workspace"
        run("git", "clone", "-q", self.repo_path, self.workspace)

        def in_workspace(argv):
            return [str(self.workspace) if a == "/workspace" else a for a in argv]

        def container_exec(name, argv, user=None, workdir=None):
            result = __import__("subprocess").run(in_workspace(argv), capture_output=True, text=True)
            if result.returncode != 0:
                raise BottleError(result.stderr)
            return result.stdout

        for patcher in (
            mock.patch.object(bottles.runtime, "exec_command", side_effect=lambda name, argv, user=None: in_workspace(argv)),
            mock.patch.object(bottles.runtime, "container_exec", side_effect=container_exec),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def host(self, *args: str) -> str:
        return run("git", "-C", self.repo_path, *args)

    def agent_commit(self, message: str) -> str:
        return self.commit(self.workspace, message)

    def ref(self, name: str) -> str:
        return self.host("rev-parse", f"refs/remotes/bottle-gradle/{name}")


class FetchTest(WorkspaceTestCase):
    def test_fetches_every_branch(self) -> None:
        run("git", "-C", self.workspace, "switch", "-q", "-c", "agent/feature")
        feature = self.agent_commit("feature work")

        result = bottles.fetch("gradle")

        self.assertEqual(self.ref("agent/feature"), feature)
        self.assertEqual(self.ref("main"), self.head)
        self.assertEqual({(u.ref, u.kind) for u in result.updates}, {
            ("refs/remotes/bottle-gradle/agent/feature", "new"), ("refs/remotes/bottle-gradle/main", "new")})
        self.assertEqual(self.host("rev-parse", "main"), self.head)  # the host's own branches are untouched
        self.assertNotIn("detached", self.host("branch", "-r"))  # on a branch: no detached ref

    def test_detached_head_gets_its_own_ref(self) -> None:
        run("git", "-C", self.workspace, "checkout", "-q", "--detach")
        detached = self.agent_commit("detached work")
        bottles.fetch("gradle")
        self.assertEqual(self.ref(f"detached/{detached[:12]}"), detached)

    def test_fast_forwards_are_updates(self) -> None:
        bottles.fetch("gradle")
        new = self.agent_commit("more")
        [update] = bottles.fetch("gradle").updates
        self.assertEqual((update.kind, update.old, update.new), ("updated", self.head, new))

    def test_up_to_date(self) -> None:
        bottles.fetch("gradle")
        self.assertEqual(bottles.fetch("gradle").updates, [])

    def test_never_deletes(self) -> None:
        run("git", "-C", self.workspace, "branch", "temp")
        bottles.fetch("gradle")
        run("git", "-C", self.workspace, "branch", "-D", "temp")
        bottles.fetch("gradle")
        self.assertEqual(self.ref("temp"), self.head)

    def test_refuses_rewritten_history(self) -> None:
        self.agent_commit("first")
        run("git", "-C", self.workspace, "switch", "-q", "-c", "other")
        other = self.agent_commit("other work")
        bottles.fetch("gradle")
        fetched_main = self.ref("main")
        run("git", "-C", self.workspace, "switch", "-q", "main")
        run("git", "-C", self.workspace, "reset", "-q", "--hard", "HEAD~1")
        self.agent_commit("rewritten")
        run("git", "-C", self.workspace, "switch", "-q", "other")
        other_next = self.agent_commit("more other work")

        with self.assertRaisesRegex(BottleError, r"rewrote history(.|\n)*bottle-gradle/main(.|\n)*bottle git fetch gradle main --force"):
            bottles.fetch("gradle")

        self.assertEqual(self.ref("main"), fetched_main)  # kept
        self.assertEqual(self.ref("other"), other_next)  # the rest still fetched
        self.assertNotEqual(other, other_next)

    def test_force_overwrites_a_single_branch(self) -> None:
        self.agent_commit("first")
        bottles.fetch("gradle")
        run("git", "-C", self.workspace, "reset", "-q", "--hard", "HEAD~1")
        rewritten = self.agent_commit("rewritten")
        with self.assertRaises(BottleError):
            bottles.fetch("gradle", "main")
        [update] = bottles.fetch("gradle", "main", force=True).updates
        self.assertEqual((update.kind, update.new), ("forced", rewritten))
        self.assertEqual(self.ref("main"), rewritten)

    def test_force_needs_a_branch(self) -> None:
        with self.assertRaisesRegex(BottleError, "--force needs a single branch"):
            bottles.fetch("gradle", force=True)
        with self.assertRaisesRegex(BottleError, "--force needs a branch; 'HEAD' isn't a branch"):
            bottles.fetch("gradle", "HEAD", force=True)

    def test_fetches_one_branch(self) -> None:
        run("git", "-C", self.workspace, "switch", "-q", "-c", "a")
        a = self.agent_commit("a")
        run("git", "-C", self.workspace, "switch", "-q", "-c", "b")
        self.agent_commit("b")
        bottles.fetch("gradle", "a")
        self.assertEqual(self.ref("a"), a)
        self.assertNotIn("bottle-gradle/b", self.host("branch", "-r"))

    def test_fetches_a_commit(self) -> None:
        wanted = self.agent_commit("wanted")
        self.agent_commit("later")
        self.assertEqual(bottles.fetch("gradle", wanted[:10]).commit, wanted)
        self.assertEqual(self.host("cat-file", "-t", wanted), "commit")
        self.assertEqual(self.host("rev-parse", "FETCH_HEAD"), wanted)

    def test_unknown_rev(self) -> None:
        with self.assertRaisesRegex(BottleError, "gradle has no branch or commit 'nope'"):
            bottles.fetch("gradle", "nope")

    def test_starts_a_stopped_bottle(self) -> None:
        with mock.patch.object(bottles, "ensure_running", wraps=bottles.ensure_running) as ensure:
            bottles.fetch("gradle")
        ensure.assert_called_once_with("gradle")

    def test_reports_branches_gone_from_the_bottle(self) -> None:
        run("git", "-C", self.workspace, "branch", "temp")
        self.assertEqual(bottles.fetch("gradle").gone, [])
        run("git", "-C", self.workspace, "branch", "-D", "temp")
        self.assertEqual(bottles.fetch("gradle").gone, ["temp"])
        self.assertEqual(self.ref("temp"), self.head)  # reported, not deleted


class DeleteGuardTest(WorkspaceTestCase):
    """delete refuses to lose work the wrapped repo doesn't have, unless forced."""

    def test_clean_bottle_deletes(self) -> None:
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})

    def test_unfetched_commits_block_delete(self) -> None:
        run("git", "-C", self.workspace, "switch", "-q", "-c", "agent/work")
        self.agent_commit("unfetched")
        with self.assertRaisesRegex(BottleError, "has work its repo doesn't: branch agent/work. To keep it, fetch with `bottle git fetch gradle`; or delete anyway with --force"):
            bottles.delete("gradle")
        self.assertIn("gradle", bottles.load())

    def test_fetching_unblocks_delete(self) -> None:
        self.agent_commit("work")
        bottles.fetch("gradle")
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})

    def test_detached_head_and_uncommitted_changes_block_delete(self) -> None:
        run("git", "-C", self.workspace, "checkout", "-q", "--detach")
        self.agent_commit("detached")
        (self.workspace / "scratch.txt").write_text("wip")
        with self.assertRaisesRegex(BottleError, "detached HEAD, uncommitted changes. To keep it, commit the changes in the bottle and fetch"):
            bottles.delete("gradle")

    def test_force_deletes_anyway(self) -> None:
        self.agent_commit("unfetched")
        bottles.delete("gradle", force=True)
        self.assertEqual(bottles.load(), {})

    def test_unable_to_check_needs_force(self) -> None:
        with mock.patch.object(bottles, "unfetched_work", side_effect=BottleError("exec failed")):
            with self.assertRaisesRegex(BottleError, "couldn't check gradle for unfetched work .*--force"):
                bottles.delete("gradle")


class ListTest(BottleTestCase):
    def test_states(self) -> None:
        fake = self.fake()
        a = bottles.create("gradle", "tools")
        b = bottles.create("gradle", "tools")
        fake.containers[b.container] = "stopped"
        self.assertEqual([(x.name, state) for x, state in bottles.list_all()], [("gradle", "running"), ("gradle-2", "stopped")])
        fake.containers.pop(a.container)
        self.assertEqual(bottles.list_all()[0][1], "missing")


class RepoBranchTest(GitTestCase):
    def test_default_start_is_origins_default_branch(self) -> None:
        origin = self.make_repo("origin")
        run("git", "-C", origin, "branch", "-m", "main", "trunk")
        clone = self.tmp / "clone"
        run("git", "clone", "-q", origin, clone)
        run("git", "-C", clone, "switch", "-q", "-c", "elsewhere")
        start = repos.default_start(repos.Repo("clone", clone))
        self.assertEqual(start, repos.Start("trunk", run("git", "-C", origin, "rev-parse", "trunk")))

    def test_default_start_without_origin_is_the_checked_out_branch(self) -> None:
        path = self.make_repo("r")
        run("git", "-C", path, "switch", "-q", "-c", "feature")
        head = self.commit(path, "on feature")
        self.assertEqual(repos.default_start(repos.Repo("r", path)), repos.Start("feature", head))

    def test_default_start_without_origin_on_a_detached_head(self) -> None:
        path = self.make_repo("r")
        first = run("git", "-C", path, "rev-parse", "HEAD")
        self.commit(path, "later")
        run("git", "-C", path, "checkout", "-q", "--detach", first)
        self.assertEqual(repos.default_start(repos.Repo("r", path)), repos.Start(None, first))

    def test_default_start_of_an_empty_repo(self) -> None:
        path = self.tmp / "empty"
        run("git", "init", "-q", path)
        with self.assertRaisesRegex(BottleError, "has no commits"):
            repos.default_start(repos.Repo("empty", path))

    def test_resolve_prefers_local_then_origin(self) -> None:
        origin = self.make_repo("origin")
        run("git", "-C", origin, "branch", "remote-only")
        clone = self.tmp / "clone"
        run("git", "clone", "-q", origin, clone)
        repo = repos.Repo("clone", clone)
        self.assertEqual(repos.resolve_branch(repo, "remote-only"), run("git", "-C", origin, "rev-parse", "remote-only"))
        local = self.commit(clone, "local work")
        self.assertEqual(repos.resolve_branch(repo, "main"), local)

    def test_objects_dir_of_a_linked_worktree_is_the_main_repos(self) -> None:
        path = self.make_repo("main-repo")
        worktree = self.tmp / "wt"
        run("git", "-C", path, "worktree", "add", "-q", worktree)
        self.assertEqual(repos.objects_dir(repos.Repo("wt", worktree)), path / ".git/objects")


if __name__ == "__main__":
    unittest.main()
