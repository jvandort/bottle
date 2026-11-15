import asyncio
import json
import os
import socket
import subprocess
import time
import unittest
from dataclasses import dataclass
from pathlib import Path

from tests.bottle.support import GitTestCase, bridge, run
from tests.support import FOLIO

MACHINE = "t-m"
HOST, HOST_ADDRESS = "bottle.host", "192.0.2.1"


@dataclass(frozen=True)
class Served:
    """One repo a bottle is served, as folio serve is told of it: its git dir, its remotes' specs, and where under
    bottle.host/git they are."""

    git_dir: Path | None
    remotes: tuple[str, ...] = ()
    at: str = ""


def folio_home() -> Path:
    return Path(os.environ["FOLIO_HOME"])


def serve(*args: str) -> str:
    """`folio serve ARGS`, its output; folio as the tests run it, on bottle-cli that isn't there."""
    done = subprocess.run([str(FOLIO), "serve", *args], capture_output=True, text=True,
                          env={**os.environ, "BOTTLE_CLI": "/no/bottle-cli"})
    assert done.returncode == 0, done.stderr
    return done.stdout


def control(message: dict) -> dict:
    """One request to folio serve, on its control socket, as folio makes one: its answer."""
    with socket.socket(socket.AF_UNIX) as s:
        s.connect(str(folio_home() / "serve.sock"))
        s.sendall(json.dumps(message).encode() + b"\n")
        return json.loads(s.makefile().readline())


def bind_message(repos: tuple[Served, ...]) -> dict:
    return {"op": "bind", "bottle": "t", "machine": MACHINE, "runner": "local", "id": "", "features": [],
            "permissions": [], "labels": {}, "repo": None, "ssh": [], "forwards": [],
            "repos": [{"git_dir": str(r.git_dir) if r.git_dir else None, "at": r.at, "remotes": list(r.remotes)}
                      for r in repos]}


class BottleHost:
    """folio serve answering a bottle with `repos` at bottle.host, and a stand-in for the bottle's machine that
    carries git's connections there (support.bridge); closed, both go."""

    def __init__(self, server) -> None:
        self.server = server

    def close(self) -> None:
        self.server.close()
        serve("stop")
        # Gone: what's stopping takes connections and closes them, until it isn't there.
        deadline = time.monotonic() + 5
        while (folio_home() / "serve.sock").exists() and time.monotonic() < deadline:
            time.sleep(0.05)

    async def wait_closed(self) -> None:
        await self.server.wait_closed()


def served_log() -> str:
    """What folio serve has logged."""
    return (folio_home() / "logs" / "serve.log").read_text()


def bind(*repos: Served) -> None:
    serve("start")
    reply = control(bind_message(repos))
    assert reply.get("ok"), reply


async def serve_bottle_host(*repos: Served) -> tuple[BottleHost, dict[str, str]]:
    """bottle.host as a bottle reaches it: folio serve, given a bottle with `repos`, and git's http://bottle.host/
    pointed at a stand-in for its machine. Returns what to close, and the environment."""
    bind(*repos)
    server, port = await bridge(folio_home() / "services" / "host.sock", HOST_ADDRESS, 80, MACHINE, HOST)
    env = {**os.environ, "GIT_CONFIG_COUNT": "1",
           "GIT_CONFIG_KEY_0": f"url.http://127.0.0.1:{port}/.insteadOf", "GIT_CONFIG_VALUE_0": "http://bottle.host/",
           "BOTTLE_HOST_PORT": str(port)}
    return BottleHost(server), env


class RemotesTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """Real git in a bottle-style workspace, talking to origin and host at bottle.host, as the decider hands it on.

    upstream stands in for the repo's real remote (e.g. GitHub); repo is the
    host repo, a clone of it with local branches of its own.
    """

    OUT = "refs/namespaces/bottle-b-out"  # the lane this bottle writes and the host reads
    IN = "refs/namespaces/bottle-b-in"    # the lane the host writes and this bottle reads

    async def asyncSetUp(self) -> None:
        self.upstream = self.make_repo("upstream")
        run("git", "-C", self.upstream, "branch", "release")
        run("git", "-C", self.upstream, "tag", "v1")
        self.repo = self.tmp / "repo"
        run("git", "clone", "-q", self.upstream, self.repo)
        run("git", "-C", self.repo, "branch", "mine")
        # What a bottle made from a registered repo is served (see bottles.default_remotes).
        remotes = ("origin:fetch=upstream", "host:fetch=heads", "work:fetch=ns/bottle-b-in,push=ns/bottle-b-out")
        self.served = Served(self.repo / ".git", remotes)
        server, self.env = await serve_bottle_host(self.served)
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

    async def test_every_fetch_pulls_through(self) -> None:
        await self.fetch()
        later = self.commit(self.upstream, "just after")
        await self.fetch()
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "origin/main"), later)

    async def test_a_failed_pull_through_still_serves_what_the_repo_has(self) -> None:
        run("git", "-C", self.repo, "remote", "set-url", "origin", str(self.tmp / "gone"))
        await self.fetch()
        self.assertIn("origin/release", self.branches())
        self.assertIn("pull-through: git fetch origin", served_log())

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
        # folio serve takes remotes only as specs, and refuses one that would push to no namespace: what
        # hooks/pre-receive would catch is never served.
        await self.work()
        reply = control({**bind_message((self.served,)),
                         "repos": [{"git_dir": str(self.repo / ".git"), "at": "", "remotes": ["work:fetch=heads,push=ns/"]}]})
        self.assertFalse(reply["ok"])
        self.assertIn("invalid remote", reply["error"])
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
        server, self.env = await serve_bottle_host(Served(self.repo / ".git", ("agent:fetch=ns/t-out,push=ns/t-out",)))
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
        said = await self.push()
        self.assertEqual(pushed, self.users())
        self.assertEqual("second\n", (self.repo / "a.txt").read_text())
        self.assertEqual(pushed, self.users("bottle/t/genie/x"))  # as if the user had fetched
        self.assertEqual(f"bottle: moved the user's genie/x to {pushed[:12]}, files and all", said)
        self.assertIn("moved the user's genie/x", served_log())

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
        curate = Path(__file__).resolve().parents[3] / "bin" / "curate"
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



class ServedRemotesTest(GitTestCase, unittest.IsolatedAsyncioTestCase):
    """Remotes a caller named itself: the same views and the same rules as the defaults, under its names."""

    async def asyncSetUp(self) -> None:
        self.upstream = self.make_repo("upstream")
        run("git", "-C", self.upstream, "branch", "release")
        self.repo = self.tmp / "repo"
        run("git", "clone", "-q", self.upstream, self.repo)
        run("git", "-C", self.repo, "branch", "mine")
        remotes = ("up:fetch=upstream", "theirs:fetch=heads", "lane:fetch=ns/t-in,push=ns/t-out", "own:fetch=ns/o,push=ns/o",)
        server, self.env = await serve_bottle_host(Served(self.repo / ".git", remotes))
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
