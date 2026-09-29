import asyncio
import os
import subprocess
import unittest
from unittest import mock

from bottle import egress, host
from tests.support import GitTestCase, run


class RemotesTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """Real git in a bottle-style workspace, talking to origin and host through the egress proxy.

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
        services = host.HostServices(self.repo / ".git", self.tmp / "view.git", bottle="b")
        self.services = services
        proxy = await egress.EgressProxy("t", egress.Policy(), services=services).start("127.0.0.1", 0)
        self.addAsyncCleanup(self._close, proxy)
        self.env = {**os.environ, "http_proxy": f"http://127.0.0.1:{proxy.sockets[0].getsockname()[1]}"}
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

    async def test_push_without_a_bottle_is_refused(self) -> None:
        # Fail closed: with no lane a push would write the host's own refs.
        await self.work()
        self.services.bottle = None
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
        reader, writer = await asyncio.open_connection("127.0.0.1", int(self.env["http_proxy"].rsplit(":", 1)[1]))
        writer.write(raw)
        response = await reader.read()
        writer.close()
        return response

    async def test_other_paths_are_not_found(self) -> None:
        for path in (b"/nope", b"/git", b"/git/other/info/refs"):
            with self.subTest(path):
                response = await self.request(b"GET http://bottle.host" + path + b" HTTP/1.1\r\n\r\n")
                self.assertTrue(response.startswith(b"HTTP/1.1 404"), response[:40])

    async def test_connect_to_the_host_is_refused(self) -> None:
        self.assertTrue((await self.request(b"CONNECT bottle.host:443 HTTP/1.1\r\n\r\n")).startswith(b"HTTP/1.1 403"))


class NoServicesTest(unittest.IsolatedAsyncioTestCase):
    async def test_a_proxy_without_services_refuses_bottle_host(self) -> None:
        proxy = await egress.EgressProxy("t", egress.Policy()).start("127.0.0.1", 0)
        reader, writer = await asyncio.open_connection("127.0.0.1", proxy.sockets[0].getsockname()[1])
        writer.write(b"GET http://bottle.host/git/origin/info/refs HTTP/1.1\r\n\r\n")
        self.assertTrue((await reader.read()).startswith(b"HTTP/1.1 403"))
        writer.close()
        proxy.close()


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


if __name__ == "__main__":
    unittest.main()
