"""Behaviours of git the design rests on. These do not touch the tool."""

import os
import subprocess
import unittest
import tempfile
from pathlib import Path

from .support import Repo


class GitAssumptions(unittest.TestCase):
    """Behaviours of git the design depends on. They do not touch the tool.

    A second index file is invisible to the first; hunk staging honours it;
    merge-tree merges and conflicts without a working tree, atomically;
    commit-tree loses authorship unless told; review refs collide on slashes;
    post-index-change fires when an editor stages; rebase refuses a dirty tree.

    If one of these fails, the design is wrong rather than the implementation.
    That is why they live in this file and run even when the tool is not found.
    """

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.r = Repo(Path(tmp.name))

    def test_a_second_index_file_is_invisible_to_the_first(self) -> None:
        """GIT_INDEX_FILE is the whole basis of 'a second staging area'."""
        self.r.write("a.txt", "A\n")
        self.r.write("b.txt", "B\n")
        self.r.commit_all("base")
        self.r.write("a.txt", "A changed\n")
        self.r.write("b.txt", "B changed\n")
        other = str(self.r.path / ".git" / "other-index")
        self.r.git("read-tree", "HEAD", env={"GIT_INDEX_FILE": other})
        self.r.git("add", "a.txt", env={"GIT_INDEX_FILE": other})
        self.assertNotEqual(self.r.index_tree(other), self.r.tree_of("HEAD"))
        self.assertEqual(self.r.index_tree(), self.r.tree_of("HEAD"), "the repo's own index moved")

    def test_hunk_level_staging_honours_a_second_index(self) -> None:
        """Approving half a file has to work, or the model loses hunk granularity."""
        self.r.write("f.txt", "".join(f"{i}\n" for i in range(1, 41)))
        self.r.commit_all("base")
        lines = [f"{i}\n" for i in range(1, 41)]
        lines[0], lines[39] = "FIRST\n", "LAST\n"     # far apart, so two hunks
        self.r.write("f.txt", "".join(lines))
        other = str(self.r.path / ".git" / "other-index")
        self.r.git("read-tree", "HEAD", env={"GIT_INDEX_FILE": other})
        subprocess.run(["git", "-C", str(self.r.path), "add", "-p"],
                       input="y\nn\n", capture_output=True, text=True,
                       env={**os.environ, "GIT_INDEX_FILE": other})
        staged = self.r.git("diff", "--cached", "HEAD", env={"GIT_INDEX_FILE": other})
        self.assertIn("+FIRST", staged)
        self.assertNotIn("+LAST", staged, "both hunks were approved; splitting failed")

    def test_merge_tree_merges_without_a_working_tree(self) -> None:
        """git merge-tree --write-tree is why fixup needs no checkout."""
        self.r.write("f.txt", "one\n")
        base = self.r.commit_all("c0")
        self.r.write("f.txt", "two\n")
        ours = self.r.commit_all("c1")
        theirs = self.r.git("commit-tree", self.r.tree_of(base), "-p", base, "-m", "sibling")
        merged = self.r.git("merge-tree", "--write-tree", f"--merge-base={base}", ours, theirs)
        self.assertTrue(merged)
        self.assertFalse(list((self.r.path / ".git").glob("MERGE_*")), "merge state leaked")

    def test_merge_tree_reports_conflicts_atomically(self) -> None:
        """On conflict: exit 1, three stages, a tree with markers, nothing written."""
        self.r.write("f.txt", "one\n")
        base = self.r.commit_all("c0")
        self.r.write("f.txt", "two\n")
        ours = self.r.commit_all("c1")
        conflicting = self.r.blob("CONFLICTING\n")
        idx = str(self.r.path / ".git" / "tmp-index")
        self.r.git("read-tree", base, env={"GIT_INDEX_FILE": idx})
        self.r.git("update-index", "--cacheinfo", f"100644,{conflicting},f.txt",
                   env={"GIT_INDEX_FILE": idx})
        theirs = self.r.git("commit-tree", self.r.index_tree(idx), "-p", base, "-m", "x")
        refs_before = self.r.git("for-each-ref")
        result = subprocess.run(
            ["git", "-C", str(self.r.path), "merge-tree", "--write-tree",
             f"--merge-base={base}", ours, theirs],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 1)
        lines = result.stdout.splitlines()
        tree, stages = lines[0], [l for l in lines[1:] if "\t" in l]
        self.assertEqual({l.split()[2] for l in stages}, {"1", "2", "3"})
        self.assertIn("<<<<<<<", self.r.file_at(tree, "f.txt"))
        self.assertEqual(self.r.git("for-each-ref"), refs_before, "a ref moved on conflict")

    def test_commit_tree_loses_authorship_unless_told(self) -> None:
        """Rebuilding the clean line must preserve the author, or it steals credit."""
        self.r.write("f.txt", "x\n")
        self.r.git("add", "-A")
        author = {"GIT_AUTHOR_NAME": "Ada", "GIT_AUTHOR_EMAIL": "ada@example.com",
                  "GIT_AUTHOR_DATE": "2020-01-02T03:04:05+00:00"}
        self.r.git("commit", "-q", "-m", "original", env=author)
        original = self.r.git("rev-parse", "HEAD")
        tree = self.r.tree_of(original)
        naive = self.r.git("commit-tree", tree, "-m", "original")
        self.assertNotEqual(self.r.git("log", "-1", "--format=%ae", naive), "ada@example.com")
        kept = self.r.git("commit-tree", tree, "-m", "original", env=author)
        self.assertEqual(self.r.git("log", "-1", "--format=%ae", kept), "ada@example.com")
        self.assertEqual(self.r.git("log", "-1", "--format=%ce", kept), "test@example.com",
                         "the committer should be whoever ran the fixup")

    def test_curate_refs_collide_on_slashes(self) -> None:
        """Why session names are encoded: two legal branch names, one impossible pair."""
        self.r.write("f.txt", "x\n")
        self.r.commit_all("c0")
        tree = self.r.tree_of("HEAD")
        self.r.git("update-ref", "refs/curate/agent/foo",
                   self.r.git("commit-tree", tree, "-m", "a"))
        result = subprocess.run(
            ["git", "-C", str(self.r.path), "update-ref", "refs/curate/agent",
             self.r.git("commit-tree", tree, "-m", "b")],
            capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot", result.stderr.lower())

    def test_post_index_change_fires_when_an_editor_stages(self) -> None:
        """The hook that makes approvals durable the instant they are made."""
        log = self.r.path / "hook.log"
        hook = self.r.path / ".git" / "hooks" / "post-index-change"
        hook.write_text(f"#!/bin/sh\necho fired >> {log}\n")
        hook.chmod(0o755)
        self.r.write("f.txt", "one\n")
        self.r.commit_all("c0")
        self.r.write("f.txt", "one\ntwo\n")
        log.write_text("")
        self.r.git("add", "f.txt")                       # staging a whole file
        patch = self.r.path / "p.patch"
        self.assertIn("fired", log.read_text())
        log.write_text("")
        self.r.git("reset", "-q")
        patch.write_text(self.r.git_raw("diff"))
        if patch.read_text().strip():
            self.r.git("apply", "--cached", str(patch))  # staging one hunk
            self.assertIn("fired", log.read_text())

    def test_rebase_refuses_a_dirty_tree_by_default(self) -> None:
        """Nothing should turn autostash on; committing is the answer."""
        self.assertEqual(self.r.git("config", "--get", "rebase.autoStash", check=False), "")
        self.r.write("f.txt", "one\n")
        self.r.commit_all("c0")
        self.r.git("branch", "other")
        self.r.write("f.txt", "two\n")
        self.r.commit_all("c1")
        self.r.git("checkout", "-q", "other")
        self.r.write("g.txt", "x\n")
        self.r.commit_all("c2")
        self.r.write("g.txt", "uncommitted\n")
        result = subprocess.run(["git", "-C", str(self.r.path), "rebase", "wip"],
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("commit or stash", (result.stderr + result.stdout).lower())
