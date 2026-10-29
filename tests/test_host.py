import asyncio
import dataclasses
from pathlib import Path
import json
import os
import subprocess
import zlib
import unittest

from bottle import egress, host
from bottle.errors import BottleError
from tests.support import GitTestCase, run


async def serve_bottle_host(services: host.HostServices) -> tuple[asyncio.Server, dict[str, str]]:
    """bottle.host as a bottle reaches it: each connection a flow to host.ADDRESS, decided and served.

    Returns the server, and an environment pointing git's http://bottle.host/ at it.
    """
    gateway = egress.Gateway("t", egress.Policy(), services=services)

    async def decider(reader, writer) -> None:
        flow = egress.Flow(host.ADDRESS, 80, await reader.read(64 * 1024))
        await gateway.serve(flow, await gateway.decide(flow), reader, writer)

    server = await asyncio.start_server(decider, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    env = {**os.environ, "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": f"url.http://127.0.0.1:{port}/.insteadOf",
           "GIT_CONFIG_VALUE_0": "http://bottle.host/", "BOTTLE_HOST_PORT": str(port)}
    return server, env


class RemotesTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """Real git in a bottle-style workspace, talking to origin and host at bottle.host, as the decider hands it on.

    upstream stands in for the repo's real remote (e.g. GitHub); repo is the
    host repo, a clone of it with local branches of its own.
    """

    OUT = "refs/namespaces/bottle-b-out"  # the lane this bottle writes and the host reads
    IN = "refs/namespaces/bottle-b-in"    # the lane the host writes and this bottle reads

    async def asyncSetUp(self) -> None:
        host._last_pull.clear()
        self.upstream = self.make_repo("upstream")
        run("git", "-C", self.upstream, "branch", "release")
        run("git", "-C", self.upstream, "tag", "v1")
        self.repo = self.tmp / "repo"
        run("git", "clone", "-q", self.upstream, self.repo)
        run("git", "-C", self.repo, "branch", "mine")
        # What a bottle made from a registered repo is served (see bottles.default_remotes).
        remotes = tuple(host.Remote.parse(spec) for spec in (
            "origin:fetch=upstream", "host:fetch=heads", "work:fetch=ns/bottle-b-in,push=ns/bottle-b-out",
        ))
        services = host.HostServices((host.Served(self.repo / ".git", remotes, view_dir=self.tmp / "view.git"),))
        self.services = services
        server, self.env = await serve_bottle_host(services)
        self.addAsyncCleanup(self._close, server)
        # The same setup as a bottle's /workspace.
        self.workspace = self.tmp / "workspace"
        run("git", "init", "-q", "-b", "main", self.workspace)
        for remote in ("origin", "host", "work"):
            run("git", "-C", self.workspace, "remote", "add", remote, f"http://bottle.host/git/{remote}")
        run("git", "-C", self.workspace, "config", "checkout.defaultRemote", "origin")

    @staticmethod
    async def _close(server) -> None:
        server.close()
        await server.wait_closed()

    async def git(self, *args, cwd=None) -> subprocess.CompletedProcess:
        return await asyncio.to_thread(
            subprocess.run, ["git", *args], cwd=cwd or self.workspace, env=self.env, capture_output=True, text=True
        )

    async def ls_remote(self, remote: str) -> set[str]:
        result = await self.git("ls-remote", f"http://bottle.host/git/{remote}")
        return {line.split("\t")[1] for line in result.stdout.splitlines()}

    async def fetch(self) -> None:
        result = await self.git("fetch", "-q", "--multiple", "origin", "host")
        self.assertEqual(result.returncode, 0, result.stderr)

    def branches(self) -> set[str]:
        return set(run("git", "-C", self.workspace, "branch", "-r", "--format=%(refname:short)").split())

    # --- origin -------------------------------------------------------------------

    async def test_origin_is_the_repos_upstream(self) -> None:
        self.assertEqual(await self.ls_remote("origin"), {"HEAD", "refs/heads/main", "refs/heads/release", "refs/tags/v1"})

    async def test_checkout_an_upstream_branch_by_name(self) -> None:
        await self.fetch()
        self.assertEqual((await self.git("checkout", "-q", "release")).returncode, 0)
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "--abbrev-ref", "@{upstream}"), "origin/release")

    async def test_pull_through_gets_upstream_commits_the_host_never_fetched(self) -> None:
        new = self.commit(self.upstream, "pushed upstream")
        await self.fetch()
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "origin/main"), new)
        self.assertEqual(run("git", "-C", self.repo, "rev-parse", "origin/main"), new)  # the host repo was fetched

    async def test_pull_through_is_throttled(self) -> None:
        await self.fetch()
        later = self.commit(self.upstream, "within the interval")
        await self.fetch()
        self.assertNotEqual(run("git", "-C", self.workspace, "rev-parse", "origin/main"), later)
        host._last_pull.clear()  # the interval passes
        await self.fetch()
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "origin/main"), later)

    async def test_a_failed_pull_through_still_serves_what_the_repo_has(self) -> None:
        run("git", "-C", self.repo, "remote", "set-url", "origin", str(self.tmp / "gone"))
        with self.assertLogs("bottle.egress", "WARNING"):
            await self.fetch()
        self.assertIn("origin/release", self.branches())

    async def test_origin_is_read_only(self) -> None:
        await self.fetch()
        await self.git("checkout", "-q", "release")
        result = await self.git("push", "origin", "HEAD:refs/heads/x")
        self.assertIn("403", result.stderr)

    # --- host: read-only ------------------------------------------------------------

    async def test_host_is_the_repos_local_branches(self) -> None:
        await self.fetch()
        self.assertTrue({"host/main", "host/mine"} <= self.branches())

    async def test_host_is_read_only(self) -> None:
        await self.work()
        result = await self.git("push", "host", "work")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("403", result.stderr)

    # --- work: two lanes ------------------------------------------------------------

    async def push(self, refspec: str | None = None, *extra: str) -> subprocess.CompletedProcess:
        return await self.git("push", *extra, "work", *([refspec] if refspec else []))

    async def work(self, message: str = "agent work") -> str:
        await self.fetch()
        await self.git("checkout", "-q", "-b", "work", "host/main")
        return self.commit(self.workspace, message)

    def pushed(self, branch: str = "work") -> str:
        return run("git", "-C", self.repo, "rev-parse", f"{self.OUT}/refs/heads/{branch}")

    def host_writes(self, branch: str, commit: str) -> None:
        """What the host's own remote does: write its lane, which the bottle reads."""
        run("git", "-C", self.repo, "update-ref", f"{self.IN}/refs/heads/{branch}", commit)

    def host_refs(self) -> set[str]:
        return set(run("git", "-C", self.repo, "for-each-ref", "--format=%(refname)").split())

    async def test_push_writes_the_bottles_own_lane(self) -> None:
        await self.work()
        self.assertEqual((await self.push("work")).returncode, 0)
        # An ordinary `git push work work` lands in the lane, with no refspec to
        # configure and nothing for the agent to know.
        self.assertEqual(self.pushed(), run("git", "-C", self.workspace, "rev-parse", "work"))

    async def test_fetch_reads_the_lane_the_host_writes(self) -> None:
        await self.work()
        mine = run("git", "-C", self.repo, "rev-parse", "mine")
        self.host_writes("review", mine)
        self.assertEqual((await self.git("fetch", "-q", "work")).returncode, 0)
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "work/review"), mine)

    async def test_the_lanes_are_separate(self) -> None:
        """What the bottle pushes is not what it reads back, or the loop is a mirror."""
        await self.work()
        await self.push("work")
        self.assertTrue(any(r.startswith(f"{self.OUT}/") for r in self.host_refs()))
        self.assertFalse(any(r.startswith(f"{self.IN}/") for r in self.host_refs()))
        # `git push` optimistically updates the remote-tracking ref, which is
        # read from the other lane, so it disagrees until a pruning fetch.
        await self.git("fetch", "-q", "--prune", "work")
        self.assertNotIn("work/work", self.branches())

    async def test_the_hosts_refs_are_out_of_the_bottles_reach(self) -> None:
        # Not refused, unreachable: every ref receive-pack resolves is inside
        # the lane, so naming main writes the lane's main, not the host's.
        before = self.host_refs()
        await self.work()
        for target in ("HEAD:main", "HEAD:mine", "HEAD:refs/tags/v1", "HEAD:refs/remotes/origin/main"):
            with self.subTest(target):
                result = await self.push(target)
                self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(run("git", "-C", self.repo, "rev-parse", "main"), run("git", "-C", self.upstream, "rev-parse", "main"))
        for ref in self.host_refs() - before:
            self.assertTrue(ref.startswith(f"{self.OUT}/"), ref)

    async def test_push_refuses_to_rewrite_what_it_has_pushed(self) -> None:
        # The user's view of the branch stays append-only: every fetch of it
        # fast-forwards, and nothing they have read changes underneath them.
        await self.work("first")
        await self.push("work")
        first = self.pushed()
        run("git", "-C", self.workspace, "reset", "-q", "--hard", "HEAD~1")
        self.commit(self.workspace, "rewritten")
        for extra in ((), ("--force",)):
            with self.subTest(force=bool(extra)):
                result = await self.push("work", *extra)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.pushed(), first)
        self.assertIn("non-fast-forward", (await self.push("work", "--force")).stderr.lower())

    async def test_push_refuses_deletes(self) -> None:
        # Only the host retires a name, since a delete takes the ref's reflog with it.
        await self.work()
        await self.push("work")
        pushed = self.pushed()
        result = await self.push(":work")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("deletion prohibited", result.stderr)
        self.assertEqual(self.pushed(), pushed)

    async def test_push_without_a_namespace_is_refused(self) -> None:
        # Fail closed: with no namespace a push would write the host's own refs.
        # Remote.parse never makes such a remote; this is the hook catching one anyway.
        await self.work()
        self.services.repos = (dataclasses.replace(self.services.repos[0], remotes=(host.Remote("work", host.HEADS, host.NS),)),)
        result = await self.push("work")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no namespace", result.stderr)
        self.assertNotIn("refs/heads/work", self.host_refs())

    async def test_the_repos_own_hooks_never_run(self) -> None:
        hook = self.repo / ".git" / "hooks" / "pre-receive"
        hook.write_text("#!/bin/sh\necho repo hook ran >&2\nexit 1\n")
        hook.chmod(0o755)
        await self.work()
        result = await self.push("work")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("repo hook ran", result.stderr)

    # --- other requests -------------------------------------------------------------

    async def request(self, raw: bytes) -> bytes:
        reader, writer = await asyncio.open_connection("127.0.0.1", int(self.env["BOTTLE_HOST_PORT"]))
        writer.write(raw)
        response = await reader.read()
        writer.close()
        return response

    async def test_other_paths_are_not_found(self) -> None:
        for path in (b"/nope", b"/git", b"/git/other/info/refs"):
            with self.subTest(path):
                response = await self.request(b"GET " + path + b" HTTP/1.1\r\nHost: bottle.host\r\n\r\n")
                self.assertTrue(response.startswith(b"HTTP/1.1 404"), response[:40])



class FollowTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """The host's checked-out branches follow a bottle's push, when it's safe, and the push says what happened.

    The host repo is set up as folio sets one up: a remote, bottle/t, reading the
    bottle's lane, and a branch genie/x tracking the lane's genie/x, checked out.
    """

    OUT = "refs/namespaces/t-out"

    async def asyncSetUp(self) -> None:
        self.repo = self.make_repo("app")
        remotes = (host.Remote.parse("agent:fetch=ns/t-out,push=ns/t-out"),)
        server, self.env = await serve_bottle_host(
            host.HostServices((host.Served(self.repo / ".git", remotes, view_dir=self.tmp / "view.git"),)))
        self.addAsyncCleanup(RemotesTest._close, server)
        for key, value in (("url", str(self.repo)), ("fetch", "+refs/heads/*:refs/remotes/bottle/t/*"),
                           ("uploadpack", "git --namespace=t-out upload-pack")):
            run("git", "-C", self.repo, "config", f"remote.bottle/t.{key}", value)
        self.workspace = self.tmp / "workspace"
        run("git", "clone", "-q", self.repo, self.workspace)
        run("git", "-C", self.workspace, "remote", "add", "agent", "http://bottle.host/git/agent")
        run("git", "-C", self.workspace, "checkout", "-q", "-b", "genie/x")
        self.write("a.txt", "first\n")
        await self.push()
        # As `git fetch bottle/t && git switch genie/x` does: the user's copy, tracking the bottle's.
        run("git", "-C", self.repo, "fetch", "-q", "bottle/t")
        run("git", "-C", self.repo, "switch", "-q", "genie/x")
        self.assertEqual("refs/remotes/bottle/t/genie/x", run("git", "-C", self.repo, "rev-parse", "--symbolic-full-name", "genie/x@{upstream}"))

    async def push(self, refspec: str = "genie/x", remote: str = "agent") -> str:
        """Push from the bottle; what the push's output said about following."""
        result = await asyncio.to_thread(subprocess.run, ["git", "push", remote, refspec], cwd=self.workspace,
                                         env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return "\n".join(line.removeprefix("remote: ").strip() for line in result.stderr.splitlines()
                         if line.startswith("remote: bottle:"))

    def write(self, path: str, text: str) -> str:
        (self.workspace / path).write_text(text)
        run("git", "-C", self.workspace, "add", path)
        run("git", "-C", self.workspace, "commit", "-q", "-m", f"write {path}")
        return run("git", "-C", self.workspace, "rev-parse", "HEAD")

    def users(self, branch: str = "genie/x") -> str:
        return run("git", "-C", self.repo, "rev-parse", branch)

    async def test_a_tracking_branch_follows_files_and_all_and_the_bottle_is_told(self) -> None:
        pushed = self.write("a.txt", "second\n")
        with self.assertLogs("bottle.egress", "INFO") as logs:
            said = await self.push()
        self.assertEqual(pushed, self.users())
        self.assertEqual("second\n", (self.repo / "a.txt").read_text())
        self.assertEqual(pushed, self.users("bottle/t/genie/x"))  # as if the user had fetched
        self.assertEqual(f"bottle: moved the user's genie/x to {pushed[:12]}, files and all", said)
        self.assertIn("moved the user's genie/x", "\n".join(logs.output))

    async def test_uncommitted_changes_elsewhere_come_along(self) -> None:
        (self.repo / "mine.txt").write_text("not committed\n")
        pushed = self.write("a.txt", "second\n")
        await self.push()
        self.assertEqual(pushed, self.users())
        self.assertEqual("not committed\n", (self.repo / "mine.txt").read_text())

    async def test_not_over_a_change_in_the_way(self) -> None:
        (self.repo / "a.txt").write_text("the user's edit\n")
        before = self.users()
        self.write("a.txt", "second\n")
        said = await self.push()
        self.assertEqual(before, self.users())
        self.assertEqual("the user's edit\n", (self.repo / "a.txt").read_text())
        self.assertEqual("bottle: didn't move the user's genie/x: Your local changes to the following files would be "
                         "overwritten by merge: a.txt", said)

    async def test_not_over_commits_of_the_users(self) -> None:
        mine = self.commit(self.repo, "the user's own")
        self.write("a.txt", "second\n")
        said = await self.push()
        self.assertEqual(mine, self.users())
        self.assertEqual("bottle: didn't move the user's genie/x: it has commits yours doesn't", said)

    async def test_not_in_the_middle_of_a_git_operation(self) -> None:
        before = self.users()
        (self.repo / ".git" / "MERGE_HEAD").write_text(before + "\n")
        self.write("a.txt", "second\n")
        said = await self.push()
        self.assertEqual(before, self.users())
        self.assertEqual("bottle: didn't move the user's genie/x: it's in the middle of a merge", said)

    async def test_not_when_the_branch_isnt_checked_out(self) -> None:
        # In curate's read mode, HEAD is curate/genie/x: the branch isn't checked out, so nothing moves.
        run("git", "-C", self.repo, "switch", "-q", "-c", "curate/genie/x")
        before = self.users()
        pushed = self.write("a.txt", "second\n")
        self.assertEqual("", await self.push())
        self.assertEqual(before, self.users())
        self.assertEqual("first\n", (self.repo / "a.txt").read_text())
        self.assertEqual(pushed, self.users("bottle/t/genie/x"))  # fetched all the same

    async def test_a_branch_youre_reading_in_curate_comes_along_by_curates_own_hook(self) -> None:
        curate = Path(__file__).resolve().parents[1] / "bin" / "curate"
        for args in (("start", "--from", "main"), ("read",)):
            result = subprocess.run([str(curate), *args], cwd=self.repo, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual("refs/heads/curate/genie/x", run("git", "-C", self.repo, "symbolic-ref", "HEAD"))
        pushed = self.write("a.txt", "second\n")
        said = await self.push()
        self.assertEqual(pushed, self.users())
        self.assertEqual("second\n", (self.repo / "a.txt").read_text())
        self.assertEqual("refs/heads/curate/genie/x", run("git", "-C", self.repo, "symbolic-ref", "HEAD"))
        self.assertIn("curate: 'genie/x' followed bottle/t/genie/x", said)

    async def test_in_whichever_worktree_has_it(self) -> None:
        run("git", "-C", self.repo, "switch", "-q", "main")
        elsewhere = self.tmp / "review"
        run("git", "-C", self.repo, "worktree", "add", "-q", elsewhere, "genie/x")
        pushed = self.write("a.txt", "second\n")
        said = await self.push()
        self.assertEqual(pushed, self.users())
        self.assertEqual("second\n", (elsewhere / "a.txt").read_text())
        self.assertEqual(f"bottle: moved the user's genie/x (in worktree review) to {pushed[:12]}, files and all", said)
        self.assertNotIn(str(self.tmp), said)  # no host paths for the bottle

    async def test_branches_tracking_anything_else_stay_put(self) -> None:
        run("git", "-C", self.repo, "branch", "--unset-upstream", "genie/x")
        before = self.users()
        self.write("a.txt", "second\n")
        self.assertEqual("", await self.push())
        self.assertEqual(before, self.users())

    async def test_the_repo_can_say_no(self) -> None:
        run("git", "-C", self.repo, "config", "bottle.follow", "false")
        before = self.users()
        self.write("a.txt", "second\n")
        self.assertEqual("", await self.push())
        self.assertEqual(before, self.users())

    async def test_ignored_files_dont_stop_it(self) -> None:
        self.write(".gitignore", ".obsidian/\n")
        await self.push()
        (self.repo / ".obsidian").mkdir()
        (self.repo / ".obsidian" / "workspace.json").write_text("{}")
        pushed = self.write("Now.md", "x\n")
        await self.push()
        self.assertEqual(pushed, self.users())

    def test_a_remote_has_no_advance_any_more(self) -> None:
        with self.assertRaises(BottleError):
            host.Remote.parse("a:fetch=ns/x,push=ns/x,advance=main")


class RoutingTest(unittest.TestCase):
    """Which mount's repo a /git/... request is for: each mount's remotes are under its path."""

    def setUp(self) -> None:
        agent = host.Remote.parse("agent:fetch=ns/a,push=ns/a")
        self.workspace = host.Served(Path("/repos/app/.git"), (agent, host.Remote.parse("origin:fetch=upstream")), "workspace")
        self.notes = host.Served(Path("/repos/notes/.git"), (agent,), "notes")
        self.nested = host.Served(Path("/repos/deep/.git"), (agent,), "workspace/deep")
        self.services = host.HostServices((self.workspace, self.notes, self.nested))

    def test_each_mount_by_its_path(self) -> None:
        for path, served, remote, rest in (
            ("/git/workspace/agent/info/refs", self.workspace, "agent", "/info/refs"),
            ("/git/workspace/origin/git-upload-pack", self.workspace, "origin", "/git-upload-pack"),
            ("/git/notes/agent/info/refs", self.notes, "agent", "/info/refs"),
            ("/git/workspace/deep/agent/info/refs", self.nested, "agent", "/info/refs"),
        ):
            with self.subTest(path):
                routed = self.services.route(path)
                assert routed is not None
                self.assertEqual((routed[0], routed[1].name, routed[2]), (served, remote, rest))

    def test_nothing_else(self) -> None:
        for path in ("/git/agent/info/refs", "/git/notes/origin/info/refs", "/git/other/agent/x", "/messages"):
            with self.subTest(path):
                self.assertIsNone(self.services.route(path))

    def test_a_bottle_from_before_mounts_keeps_its_remotes_at_git(self) -> None:
        legacy = host.HostServices((host.Served(Path("/repos/app/.git"), self.workspace.remotes, ""),))
        routed = legacy.route("/git/agent/info/refs")
        assert routed is not None
        self.assertEqual(routed[1].name, "agent")

    def test_a_repo_thats_gone_serves_nothing(self) -> None:
        gone = host.HostServices((host.Served(None, self.workspace.remotes, "workspace"),))
        self.assertIsNone(gone.route("/git/workspace/agent/info/refs"))


class MessagesTest(unittest.IsolatedAsyncioTestCase):
    """A bottle's messages to other bottles, POSTed to http://bottle.host/messages, as the decider hands them on."""

    async def asyncSetUp(self) -> None:
        self.delivered: list[tuple[str, str, str]] = []
        self.refusal: host.ServiceError | None = None

        async def deliver(sender: str, to: str, text: str) -> None:
            if self.refusal:
                raise self.refusal
            self.delivered.append((sender, to, text))

        self.asked: list[str] = []

        async def peers(asker: str) -> list[dict]:
            if self.refusal:
                raise self.refusal
            self.asked.append(asker)
            return [{"name": "app-1a2b3c4d-foo", "repo": "app", "thread": "foo"}]

        server, self.env = await serve_bottle_host(host.HostServices(bottle="b", deliver=deliver, peers=peers))
        self.addAsyncCleanup(self._close, server)

    async def _close(self, server: asyncio.Server) -> None:
        server.close()
        await server.wait_closed()

    async def post(self, body: bytes, method: bytes = b"POST") -> bytes:
        reader, writer = await asyncio.open_connection("127.0.0.1", int(self.env["BOTTLE_HOST_PORT"]))
        writer.write(method + b" /messages HTTP/1.1\r\nHost: bottle.host\r\nContent-Type: application/json\r\n"
                     b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
        response = await reader.read()
        writer.close()
        return response

    async def test_a_message_is_delivered_as_from_the_bottle_that_sent_it(self) -> None:
        response = await self.post(json.dumps({"to": "broker", "text": "done: genie/x-2"}).encode())
        self.assertTrue(response.startswith(b"HTTP/1.1 200"), response[:40])
        self.assertEqual([("b", "broker", "done: genie/x-2")], self.delivered)

    async def test_an_undelivered_message_answers_with_why(self) -> None:
        self.refusal = host.ServiceError(504, "Gateway Timeout", "broker has no session that took the message within 10s")
        response = await self.post(json.dumps({"to": "broker", "text": "hi"}).encode())
        self.assertTrue(response.startswith(b"HTTP/1.1 504"), response[:40])
        self.assertIn(b"broker has no session", response)

    async def test_a_bad_message_is_refused_saying_why(self) -> None:
        for body, why in (
            (b"not json", b"is JSON"),
            (b"[]", b"JSON object"),
            (json.dumps({"text": "hi"}).encode(), b"needs to"),
            (json.dumps({"to": " ", "text": "hi"}).encode(), b"needs to"),
            (json.dumps({"to": 1, "text": "hi"}).encode(), b"needs to"),
            (json.dumps({"to": "broker"}).encode(), b"needs text"),
            (json.dumps({"to": "broker", "text": "hi", "from": "other"}).encode(), b"not from"),
            (json.dumps({"to": "broker", "text": "x" * host.MAX_MESSAGE}).encode(), b"at most"),
        ):
            with self.subTest(body[:40]):
                response = await self.post(body)
                self.assertTrue(response.startswith(b"HTTP/1.1 400"), response[:40])
                self.assertIn(why, response)
        self.assertEqual([], self.delivered)

    async def test_only_post(self) -> None:
        response = await self.post(b"", method=b"GET")
        self.assertTrue(response.startswith(b"HTTP/1.1 405"), response[:40])

    async def get_bottles(self) -> bytes:
        reader, writer = await asyncio.open_connection("127.0.0.1", int(self.env["BOTTLE_HOST_PORT"]))
        writer.write(b"GET /bottles HTTP/1.1\r\nHost: bottle.host\r\n\r\n")
        response = await reader.read()
        writer.close()
        return response

    async def test_the_running_bottles_are_listed_as_json(self) -> None:
        response = await self.get_bottles()
        self.assertTrue(response.startswith(b"HTTP/1.1 200"), response[:40])
        self.assertIn(b"Content-Type: application/json", response)
        self.assertEqual([{"name": "app-1a2b3c4d-foo", "repo": "app", "thread": "foo"}],
                         json.loads(response.partition(b"\r\n\r\n")[2]))
        self.assertEqual(["b"], self.asked)

    async def test_a_refused_listing_says_why(self) -> None:
        self.refusal = host.ServiceError(403, "Forbidden", "listing bottles needs the list-bottles permission")
        response = await self.get_bottles()
        self.assertTrue(response.startswith(b"HTTP/1.1 403"), response[:40])
        self.assertIn(b"needs the list-bottles permission", response)

    async def test_services_that_deliver_nothing_take_no_messages(self) -> None:
        server, env = await serve_bottle_host(host.HostServices(bottle="b"))
        self.addAsyncCleanup(self._close, server)
        self.env = env
        response = await self.post(json.dumps({"to": "broker", "text": "hi"}).encode())
        self.assertTrue(response.startswith(b"HTTP/1.1 404"), response[:40])


class NoServicesTest(unittest.IsolatedAsyncioTestCase):
    async def test_a_gateway_without_services_refuses_bottle_host(self) -> None:
        with self.assertRaises(egress.Refused):
            await egress.Gateway("t", egress.Policy()).decide(egress.Flow(host.ADDRESS, 80, b"GET / HTTP/1.1\r\n\r\n"))


class FakeWriter:
    """The little of asyncio.StreamWriter that _relay_cgi uses: write and drain."""

    def __init__(self) -> None:
        self.written = b""

    def write(self, data: bytes) -> None:
        self.written += data

    async def drain(self) -> None:
        pass


class ReadBodyTest(unittest.IsolatedAsyncioTestCase):
    async def body(self, raw: bytes, headers: dict[str, str]) -> bytes:
        reader = asyncio.StreamReader()
        reader.feed_data(raw)
        reader.feed_eof()
        return await host.read_body(reader, headers)

    async def test_content_length(self) -> None:
        self.assertEqual(await self.body(b"hello", {"content-length": "5"}), b"hello")

    async def test_chunked(self) -> None:
        raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"
        self.assertEqual(await self.body(raw, {"transfer-encoding": "chunked"}), b"hello world")

    async def test_no_body(self) -> None:
        self.assertEqual(await self.body(b"", {}), b"")

    async def test_chunked_carries_a_binary_body_intact(self) -> None:
        """A packfile is binary, and dechunking must not touch a byte of it."""
        payload = bytes(range(256)) * 4
        raw = b"%x\r\n%s\r\n0\r\n\r\n" % (len(payload), payload)
        self.assertEqual(await self.body(raw, {"transfer-encoding": "chunked"}), payload)

    async def test_gzip_stays_compressed_here(self) -> None:
        """read_body only frames; the gzip apt sends is decompressed in _git,
        after dechunking, so read_body hands back the compressed bytes as they
        arrived rather than looking inside them."""
        blob = zlib.compress(b"payload")
        raw = b"%x\r\n%s\r\n0\r\n\r\n" % (len(blob), blob)
        self.assertEqual(await self.body(raw, {"transfer-encoding": "chunked",
                                               "content-encoding": "gzip"}), blob)

    async def test_chunked_over_the_limit_is_refused(self) -> None:
        big = b"x" * (host.MAX_BODY + 1)
        raw = b"%x\r\n%s\r\n0\r\n\r\n" % (len(big), big)
        with self.assertRaises(ValueError):
            await self.body(raw, {"transfer-encoding": "chunked"})

    async def test_content_length_over_the_limit_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            await self.body(b"", {"content-length": str(host.MAX_BODY + 1)})


class RelayCgiTest(unittest.IsolatedAsyncioTestCase):
    """git http-backend speaks CGI; _relay_cgi turns that into an HTTP response.

    The corner it has to get right is the boundary between the CGI header block
    and a binary body: parse it wrong and either the status is lost or the
    packfile is corrupted.
    """

    async def relay(self, output: bytes) -> bytes:
        writer = FakeWriter()
        await host._relay_cgi(output, writer)
        return writer.written

    async def head_and_body(self, output: bytes) -> tuple[bytes, bytes]:
        head, _, body = (await self.relay(output)).partition(b"\r\n\r\n")
        return head, body

    async def test_status_becomes_the_http_status_line(self) -> None:
        head, _ = await self.head_and_body(
            b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nnope")
        self.assertTrue(head.startswith(b"HTTP/1.1 404 Not Found\r\n"))

    async def test_a_missing_status_defaults_to_200(self) -> None:
        head, _ = await self.head_and_body(b"Content-Type: text/plain\r\n\r\nhi")
        self.assertTrue(head.startswith(b"HTTP/1.1 200 OK\r\n"))

    async def test_bare_lf_separators_are_accepted(self) -> None:
        """CGI programs may use LF where HTTP wants CRLF."""
        head, body = await self.head_and_body(
            b"Content-Type: text/plain\nStatus: 201 Created\n\nmade")
        self.assertTrue(head.startswith(b"HTTP/1.1 201 Created\r\n"))
        self.assertEqual(body, b"made")

    async def test_a_binary_body_survives_byte_for_byte(self) -> None:
        payload = bytes(range(256)) * 8      # every byte, including CR, LF, NUL
        head, body = await self.head_and_body(
            b"Content-Type: application/x-git-upload-pack-result\r\n\r\n" + payload)
        self.assertEqual(body, payload)
        self.assertIn(b"Content-Length: %d\r\n" % len(payload), head)

    async def test_framing_headers_are_not_duplicated(self) -> None:
        """If http-backend sets its own Content-Length or Connection, the relay
        must not emit a second copy -- one framing is set here."""
        response = await self.relay(
            b"Content-Type: text/plain\r\nContent-Length: 99\r\nConnection: keep-alive\r\n\r\nok")
        self.assertEqual(response.count(b"Content-Length:"), 1)
        self.assertEqual(response.count(b"Connection:"), 1)
        self.assertIn(b"Content-Length: 2\r\n", response)   # ours, the real length
        self.assertIn(b"Connection: close\r\n", response)


if __name__ == "__main__":
    unittest.main()


class RemoteParseTest(unittest.TestCase):
    def test_parses(self) -> None:
        for spec, remote in (
            ("origin:fetch=upstream", host.Remote("origin", host.UPSTREAM)),
            ("host:fetch=heads", host.Remote("host", host.HEADS)),
            ("work:fetch=ns/t-in,push=ns/t-out", host.Remote("work", "ns/t-in", "ns/t-out")),
            ("mine:push=ns/x,fetch=ns/x", host.Remote("mine", "ns/x", "ns/x")),
            ("up:fetch=upstream,push=ns/x", host.Remote("up", host.UPSTREAM, "ns/x")),
        ):
            with self.subTest(spec):
                self.assertEqual(host.Remote.parse(spec), remote)

    def test_round_trips(self) -> None:
        for spec in ("origin:fetch=upstream", "work:fetch=ns/t-in,push=ns/t-out"):
            self.assertEqual(host.Remote.parse(spec).spec, spec)

    def test_namespaces(self) -> None:
        remote = host.Remote.parse("work:fetch=ns/t-in,push=ns/t-out")
        self.assertEqual((remote.fetch_namespace, remote.push_namespace), ("t-in", "t-out"))
        self.assertEqual((host.Remote("o", host.UPSTREAM).fetch_namespace, host.Remote("o", host.UPSTREAM).push_namespace), (None, None))

    def test_refuses(self) -> None:
        for spec in (
            "origin", "origin:", ":fetch=heads", "a/b:fetch=heads", "x:push=ns/y", "x:fetch=everything",
            "x:fetch=ns/", "x:fetch=ns/a/b", "x:fetch=heads,push=heads", "x:fetch=heads,fetch=heads",
            "x:fetch=heads,pull=ns/y", "x:fetch=heads,push=ns/-y",
        ):
            with self.subTest(spec), self.assertRaises(host.BottleError):
                host.Remote.parse(spec)


class ServedRemotesTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """Remotes a caller named itself: the same views and the same rules as the defaults, under its names."""

    async def asyncSetUp(self) -> None:
        host._last_pull.clear()
        self.upstream = self.make_repo("upstream")
        run("git", "-C", self.upstream, "branch", "release")
        self.repo = self.tmp / "repo"
        run("git", "clone", "-q", self.upstream, self.repo)
        run("git", "-C", self.repo, "branch", "mine")
        remotes = tuple(host.Remote.parse(spec) for spec in (
            "up:fetch=upstream", "theirs:fetch=heads", "lane:fetch=ns/t-in,push=ns/t-out", "own:fetch=ns/o,push=ns/o",
        ))
        services = host.HostServices((host.Served(self.repo / ".git", remotes, view_dir=self.tmp / "view.git"),))
        server, self.env = await serve_bottle_host(services)
        self.addAsyncCleanup(RemotesTest._close, server)
        self.workspace = self.tmp / "workspace"
        run("git", "init", "-q", "-b", "main", self.workspace)
        for remote in ("up", "theirs", "lane", "own", "origin", "work"):
            run("git", "-C", self.workspace, "remote", "add", remote, f"http://bottle.host/git/{remote}")

    async def git(self, *args) -> subprocess.CompletedProcess:
        return await asyncio.to_thread(
            subprocess.run, ["git", *args], cwd=self.workspace, env=self.env, capture_output=True, text=True
        )

    async def ls_remote(self, remote: str) -> set[str]:
        result = await self.git("ls-remote", f"http://bottle.host/git/{remote}")
        self.assertEqual(result.returncode, 0, result.stderr)
        return {line.split("\t")[1] for line in result.stdout.splitlines()}

    async def test_each_view(self) -> None:
        self.assertEqual(await self.ls_remote("up"), {"HEAD", "refs/heads/main", "refs/heads/release"})
        self.assertTrue({"refs/heads/main", "refs/heads/mine"} <= await self.ls_remote("theirs"))
        self.assertEqual(await self.ls_remote("lane"), set())  # nothing in t-in yet
        run("git", "-C", self.repo, "update-ref", "refs/namespaces/t-in/refs/heads/handed", "HEAD")
        self.assertEqual(await self.ls_remote("lane"), {"refs/heads/handed"})

    async def test_names_it_wasnt_given_arent_served(self) -> None:
        for remote in ("origin", "work"):
            with self.subTest(remote):
                result = await self.git("ls-remote", f"http://bottle.host/git/{remote}")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no such bottle.host service", result.stderr)

    async def test_a_push_lands_in_its_namespace_append_only(self) -> None:
        self.assertEqual((await self.git("fetch", "-q", "theirs")).returncode, 0)
        await self.git("checkout", "-q", "-b", "agent", "theirs/main")
        first = self.commit(self.workspace, "agent work")
        result = await self.git("push", "lane", "agent")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(run("git", "-C", self.repo, "rev-parse", "refs/namespaces/t-out/refs/heads/agent"), first)
        self.assertNotIn("refs/heads/agent", run("git", "-C", self.repo, "for-each-ref", "--format=%(refname)").split())
        await self.git("reset", "-q", "--hard", "HEAD~")
        self.commit(self.workspace, "a rewrite")
        rewrite = await self.git("push", "--force", "lane", "agent")
        self.assertNotEqual(rewrite.returncode, 0)
        self.assertEqual(run("git", "-C", self.repo, "rev-parse", "refs/namespaces/t-out/refs/heads/agent"), first)
        delete = await self.git("push", "lane", ":agent")
        self.assertIn("deletion prohibited", delete.stderr)

    async def test_a_remote_that_fetches_what_it_pushes_is_ordinary_git(self) -> None:
        """Pushing, then seeing the push, tracking it, and git status saying where you are."""
        await self.git("fetch", "-q", "theirs")
        await self.git("checkout", "-q", "-b", "agent", "theirs/main")
        first = self.commit(self.workspace, "agent work")
        result = await self.git("push", "-q", "-u", "own", "agent")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "own/agent"), first)
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "--abbrev-ref", "@{u}"), "own/agent")
        self.commit(self.workspace, "more")
        status = (await self.git("status", "-sb")).stdout.splitlines()[0]
        self.assertEqual(status, "## agent...own/agent [ahead 1]")
        self.assertEqual((await self.git("push", "-q")).returncode, 0)
        self.assertEqual((await self.git("pull", "-q")).returncode, 0)  # nothing new, and nothing refused

    async def test_read_only_remotes_say_where_to_push(self) -> None:
        await self.git("fetch", "-q", "theirs")
        await self.git("checkout", "-q", "-b", "agent", "theirs/main")
        self.commit(self.workspace, "agent work")
        for remote in ("up", "theirs"):
            with self.subTest(remote):
                result = await self.git("push", remote, "agent")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("403", result.stderr)
