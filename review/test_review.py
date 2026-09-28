"""Tests for the two-line review tool, written before the tool exists.

See README.md for the model and IMPLEMENTATION.md for the mechanics. This file is the executable half of those documents: every scenario
below was worked through by hand while designing them, and an implementation is
finished when these pass.

Run:      REVIEW_BIN=/path/to/review python3 -m unittest test_review -v
Run only  the assumptions about git, which pass today against git 2.38+:
          python3 -m unittest test_review.GitAssumptions -v

THE CONTRACT UNDER TEST

    review start --from <base> [--clean <branch>]
    review write | review review          switch modes
    review status [--porcelain]
    review fixup <commit> [--continue | --abort]
    review resolve
    review list | review drop <branch>

Exit codes: 0 success, 1 refused or error, 2 a fixup is paused on a conflict.

`status --porcelain` prints key=value lines, one per line, including at least:

    mode=write|review|away
    clean=<ref>            working=<ref>
    approved=<tree oid>    outstanding=<count of unapproved paths>
    consistent=yes|no      fixup=none|pending

Approving is not a verb. The editor stages, so the tests stage: `git add` for a
whole file, `git apply --cached` for one hunk. That is exactly what an IDE's
staging UI does, and if the tool ever needs its own approve command, the model
has gone wrong.
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REVIEW = os.environ.get("REVIEW_BIN", "review")
HAVE_REVIEW = shutil.which(REVIEW) is not None or os.path.isfile(REVIEW)


class Repo:
    """A throwaway git repository, driven with plumbing rather than checkouts.

    Histories are built with commit-tree and update-index so the tests say
    exactly what they mean and never depend on what happens to be checked out.
    """

    def __init__(self, path: Path):
        self.path = path
        self.git("init", "-q", "-b", "wip", ".")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.com")

    def git(self, *args: str, check: bool = True, env: dict | None = None) -> str:
        result = subprocess.run(
            ["git", "-C", str(self.path), *args],
            capture_output=True, text=True,
            env={**os.environ, **(env or {})},
        )
        if check and result.returncode != 0:
            raise AssertionError(f"git {' '.join(args)} failed:\n{result.stderr}")
        return result.stdout.strip()

    def git_raw(self, *args: str) -> str:
        """git output verbatim -- patches and other things a strip() would corrupt."""
        return subprocess.run(["git", "-C", str(self.path), *args],
                              capture_output=True, text=True, check=True).stdout

    def review(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run([REVIEW, *args], cwd=self.path, capture_output=True, text=True)

    def status_fields(self) -> dict[str, str]:
        out = self.review("status", "--porcelain")
        assert out.returncode == 0, out.stderr
        return dict(line.split("=", 1) for line in out.stdout.splitlines() if "=" in line)

    # --- building content -----------------------------------------------------

    def write(self, name: str, text: str) -> None:
        (self.path / name).write_text(text)

    def read(self, name: str) -> str:
        return (self.path / name).read_text()

    def blob(self, text: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.path), "hash-object", "-w", "--stdin"],
            input=text, capture_output=True, text=True,
        ).stdout.strip()

    def commit_all(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def stage_hunk(self, path: str, old: str, new: str) -> None:
        """Approve part of a file: what `git apply --cached` does for one hunk."""
        self.git("update-index", "--cacheinfo", f"100644,{self.blob(new)},{path}")

    # --- inspecting -----------------------------------------------------------

    def tree_of(self, rev: str) -> str:
        return self.git("rev-parse", f"{rev}^{{tree}}")

    def index_tree(self, index: str | None = None) -> str:
        env = {"GIT_INDEX_FILE": index} if index else {}
        return self.git("write-tree", env=env)

    def log(self, rev: str) -> list[str]:
        out = self.git("log", "--format=%s", rev)
        return out.splitlines() if out else []

    def short_status(self) -> list[str]:
        out = self.git("status", "--short")
        return out.splitlines() if out else []

    def file_at(self, rev: str, path: str) -> str:
        return self.git("cat-file", "blob", f"{rev}:{path}")

    def mtimes(self) -> dict[str, float]:
        return {
            str(p.relative_to(self.path)): p.stat().st_mtime
            for p in self.path.rglob("*")
            if p.is_file() and ".git" not in p.parts
        }


@unittest.skipUnless(HAVE_REVIEW, f"{REVIEW} not found; set REVIEW_BIN to the tool under test")
class ReviewTestCase(unittest.TestCase):
    """A repository with a working line ahead of a base, ready to review."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name) / "r"
        root.mkdir(parents=True)
        self.repo = Repo(root)

    def make_base(self) -> str:
        self.repo.write("f.txt", "one\ntwo\nthree\n")
        return self.repo.commit_all("base")

    def start(self, base: str) -> None:
        result = self.repo.review("start", "--from", base)
        self.assertEqual(result.returncode, 0, result.stderr)


# ---------------------------------------------------------------------------
# Assumptions about git that the whole design rests on.
#
# These do not exercise the tool at all. They exist because if any of them stops
# being true, the design is wrong rather than the implementation, and that
# should fail loudly and in an obvious place.
# ---------------------------------------------------------------------------

class GitAssumptions(unittest.TestCase):
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

    def test_review_refs_collide_on_slashes(self) -> None:
        """Why session names are encoded: two legal branch names, one impossible pair."""
        self.r.write("f.txt", "x\n")
        self.r.commit_all("c0")
        tree = self.r.tree_of("HEAD")
        self.r.git("update-ref", "refs/review/agent/foo",
                   self.r.git("commit-tree", tree, "-m", "a"))
        result = subprocess.run(
            ["git", "-C", str(self.r.path), "update-ref", "refs/review/agent",
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


# ---------------------------------------------------------------------------
# start
# ---------------------------------------------------------------------------

class Start(ReviewTestCase):
    def test_creates_a_clean_line_at_the_base(self) -> None:
        base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(base)
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "write")
        self.assertEqual(self.repo.git("rev-parse", fields["clean"]), base)

    def test_refuses_on_a_detached_head(self) -> None:
        base = self.make_base()
        self.repo.git("checkout", "-q", "--detach")
        self.assertEqual(self.repo.review("start", "--from", base).returncode, 1)

    def test_refuses_to_start_twice(self) -> None:
        base = self.make_base()
        self.start(base)
        self.assertEqual(self.repo.review("start", "--from", base).returncode, 1)


# ---------------------------------------------------------------------------
# Switching modes
# ---------------------------------------------------------------------------

class ModeSwitch(ReviewTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.write("junk.txt", "debug\n")
        self.tip = self.repo.commit_all("their work")
        self.start(self.base)

    def test_review_mode_points_head_at_the_clean_line(self) -> None:
        self.repo.review("review")
        fields = self.repo.status_fields()
        self.assertEqual(fields["mode"], "review")
        self.assertEqual(self.repo.git("symbolic-ref", "HEAD"), fields["clean"])

    def test_switching_does_not_touch_a_single_file(self) -> None:
        """The property the whole design rests on: no files move, so no reindex."""
        before = self.repo.mtimes()
        self.repo.review("review")
        self.assertEqual(self.repo.mtimes(), before)
        self.repo.review("write")
        self.assertEqual(self.repo.mtimes(), before)

    def test_unstaged_in_review_mode_is_the_review_to_do_list(self) -> None:
        self.repo.review("review")
        self.assertEqual(sorted(self.repo.short_status()), [" M f.txt", "?? junk.txt"])

    def test_write_mode_hides_the_review_and_shows_only_your_own_edits(self) -> None:
        self.repo.review("write")
        self.assertEqual(self.repo.short_status(), [])
        self.repo.write("f.txt", "ONE\ntwo\nMY EDIT\n")
        self.assertEqual(self.repo.short_status(), [" M f.txt"])

    def test_each_mode_keeps_its_own_staged_set(self) -> None:
        self.repo.review("write")
        self.repo.write("mine.txt", "mine\n")
        self.repo.git("add", "mine.txt")
        self.repo.review("review")
        self.repo.git("add", "f.txt")
        self.repo.review("write")
        self.assertIn("A  mine.txt", self.repo.short_status(), "write staging was clobbered")


# ---------------------------------------------------------------------------
# Approving, and what the clean line becomes
# ---------------------------------------------------------------------------

class Approving(ReviewTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.write("junk.txt", "debug print\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.review("review")

    def test_committing_in_review_mode_extends_the_clean_line(self) -> None:
        self.repo.git("add", "f.txt")
        self.repo.git("commit", "-q", "-m", "Rename the thing")
        clean = self.repo.status_fields()["clean"]
        self.assertEqual(self.repo.log(clean), ["Rename the thing", "base"])
        self.assertEqual(self.repo.file_at(clean, "f.txt"), "ONE\ntwo\nthree\n")

    def test_what_is_never_approved_never_reaches_the_clean_line(self) -> None:
        """debug junk needs no stripping step; it simply never gets in."""
        self.repo.git("add", "f.txt")
        self.repo.git("commit", "-q", "-m", "Rename the thing")
        clean = self.repo.status_fields()["clean"]
        self.assertNotIn("junk.txt", self.repo.git("ls-tree", "--name-only", "-r", clean))
        self.assertEqual(self.repo.short_status(), ["?? junk.txt"])

    def test_approving_leaves_the_working_lines_own_index_alone(self) -> None:
        self.repo.git("add", "f.txt")
        self.repo.review("write")
        self.assertEqual(self.repo.short_status(), [])

    def test_your_own_edits_are_not_approved_just_because_you_wrote_them(self) -> None:
        """Reviewing your own code is the point; nothing auto-approves."""
        self.repo.review("write")
        self.repo.write("f.txt", "ONE\ntwo\nMY EDIT\n")
        self.repo.commit_all("my fix")
        self.repo.review("review")
        self.assertIn(" M f.txt", self.repo.short_status())

    def test_the_review_is_done_when_nothing_is_outstanding(self) -> None:
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "Everything")
        fields = self.repo.status_fields()
        self.assertEqual(fields["outstanding"], "0")
        self.assertEqual(self.repo.tree_of(fields["clean"]), self.repo.index_tree())


# ---------------------------------------------------------------------------
# The working line moving underneath a review
# ---------------------------------------------------------------------------

class WorkingLineMoves(ReviewTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.review("review")
        self.repo.git("add", "f.txt")
        self.approved = self.repo.index_tree()

    def test_approvals_survive_a_commit_on_the_working_line(self) -> None:
        self.repo.review("write")
        self.repo.write("g.txt", "new\n")
        self.repo.commit_all("more")
        self.repo.review("review")
        self.assertEqual(self.repo.index_tree(), self.approved)
        self.assertEqual(self.repo.short_status(), ["?? g.txt"])

    def test_approvals_survive_a_rewritten_history(self) -> None:
        """Content-based approval is why rebasing and squashing cost nothing."""
        self.repo.review("write")
        self.repo.git("commit", "-q", "--amend", "-m", "their work, reworded")
        self.repo.review("review")
        self.assertEqual(self.repo.index_tree(), self.approved)
        self.assertEqual(self.repo.short_status(), [])

    def test_a_re_touched_file_comes_back_unapproved(self) -> None:
        self.repo.review("write")
        self.repo.write("f.txt", "ONE\nTWO\nthree\n")
        self.repo.commit_all("changed it again")
        self.repo.review("review")
        self.assertEqual(self.repo.short_status(), [" M f.txt"])
        self.assertNotIn("ONE", self.repo.git("diff"), "the approved line reappeared")

    def test_only_the_working_line_and_the_tree_move(self) -> None:
        clean_before = self.repo.git("rev-parse", self.repo.status_fields()["clean"])
        self.repo.review("write")
        self.repo.write("g.txt", "new\n")
        self.repo.commit_all("more")
        self.repo.review("review")
        self.assertEqual(self.repo.git("rev-parse", self.repo.status_fields()["clean"]),
                         clean_before)


# ---------------------------------------------------------------------------
# fixup
# ---------------------------------------------------------------------------

class Fixup(ReviewTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.repo.write("a.txt", "A\n")
        self.base = self.repo.commit_all("base")
        self.repo.write("a.txt", "A\nA more\n")
        self.repo.write("b.txt", "B\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.review("review")
        self.repo.stage_hunk("a.txt", "A\nA more\n", "A\n")   # approve part of a.txt
        self.repo.git("commit", "-q", "-m", "Add feature A")
        self.repo.git("add", "b.txt")
        self.repo.git("commit", "-q", "-m", "Add feature B")
        self.clean = self.repo.status_fields()["clean"]
        self.target = self.repo.git("rev-parse", f"{self.clean}~1")

    def test_absorbs_rather_than_appends(self) -> None:
        self.repo.git("add", "a.txt")
        self.assertEqual(self.repo.review("fixup", self.target).returncode, 0)
        self.assertEqual(self.repo.log(self.clean), ["Add feature B", "Add feature A", "base"])
        self.assertEqual(self.repo.file_at(f"{self.clean}~1", "a.txt"), "A\nA more\n")

    def test_never_touches_the_working_tree(self) -> None:
        """No checkout, no stash: the whole point of doing it in the object database."""
        before = self.repo.mtimes()
        self.repo.git("add", "a.txt")
        self.repo.review("fixup", self.target)
        self.assertEqual(self.repo.mtimes(), before)

    def test_preserves_authorship_of_replayed_commits(self) -> None:
        authors_before = self.repo.git("log", "--format=%an <%ae> %aI", self.clean)
        self.repo.git("add", "a.txt")
        self.repo.review("fixup", self.target)
        self.assertEqual(self.repo.git("log", "--format=%an <%ae> %aI", self.clean),
                         authors_before)

    def test_amending_the_tip_needs_no_replay(self) -> None:
        self.repo.git("add", "a.txt")
        tip = self.repo.git("rev-parse", self.clean)
        self.assertEqual(self.repo.review("fixup", tip).returncode, 0)
        self.assertEqual(len(self.repo.log(self.clean)), 3)

    def test_refuses_a_target_that_is_not_on_the_clean_line(self) -> None:
        self.repo.git("add", "a.txt")
        stranger = self.repo.git("rev-parse", self.repo.status_fields()["working"])
        self.assertEqual(self.repo.review("fixup", stranger).returncode, 1)

    def test_refuses_when_nothing_is_approved_to_fix_up(self) -> None:
        result = self.repo.review("fixup", self.target)
        self.assertEqual(result.returncode, 1)
        self.assertIn("nothing", (result.stdout + result.stderr).lower())

    def test_refuses_outside_review_mode(self) -> None:
        self.repo.review("write")
        self.assertEqual(self.repo.review("fixup", self.target).returncode, 1)

    def test_handles_a_root_commit_as_the_target(self) -> None:
        self.repo.git("add", "a.txt")
        root = self.repo.git("rev-list", "--max-parents=0", self.clean)
        result = self.repo.review("fixup", root)
        self.assertIn(result.returncode, (0, 2), result.stderr)
        self.assertEqual(self.repo.git("rev-list", "--count", "--max-parents=0", self.clean), "1")


class FixupConflicts(ReviewTestCase):
    """Folding into an old commit conflicts when a later one touched the same lines.

    That also means it will conflict again on replay: two resolutions, not one.
    Inherent, and the tool should say so rather than be surprised.
    """

    def setUp(self) -> None:
        super().setUp()
        self.repo.write("f.txt", "one\n")
        self.base = self.repo.commit_all("base")
        self.repo.write("f.txt", "four\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.review("review")
        self.repo.stage_hunk("f.txt", "four\n", "two\n")
        self.repo.git("commit", "-q", "-m", "A")
        self.repo.stage_hunk("f.txt", "four\n", "three\n")
        self.repo.git("commit", "-q", "-m", "B")
        self.clean = self.repo.status_fields()["clean"]
        self.target = self.repo.git("rev-parse", f"{self.clean}~1")
        self.repo.git("add", "f.txt")

    def test_reports_a_conflict_without_moving_anything(self) -> None:
        before = self.repo.git("for-each-ref")
        tree_before = self.repo.mtimes()
        result = self.repo.review("fixup", self.target)
        self.assertEqual(result.returncode, 2)
        self.assertIn("f.txt", result.stdout + result.stderr)
        self.assertEqual(self.repo.git("for-each-ref"), before, "a ref moved on conflict")
        self.assertEqual(self.repo.mtimes(), tree_before, "the working tree was touched")

    def test_warns_that_a_later_commit_will_conflict_too(self) -> None:
        result = self.repo.review("fixup", self.target)
        self.assertIn("B", result.stdout + result.stderr,
                      "should name the later commit that also changes f.txt")

    def test_status_says_a_fixup_is_pending(self) -> None:
        self.repo.review("fixup", self.target)
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")

    def test_resolve_offers_all_three_stages(self) -> None:
        self.repo.review("fixup", self.target)
        self.repo.review("resolve")
        scratch = self.repo.path / ".git" / "review"
        names = {p.name for p in scratch.rglob("f.txt.*")}
        self.assertEqual(names, {"f.txt.BASE", "f.txt.LOCAL", "f.txt.REMOTE"})

    def test_abort_leaves_every_ref_exactly_where_it_was(self) -> None:
        before = self.repo.git("for-each-ref")
        self.repo.review("fixup", self.target)
        self.assertEqual(self.repo.review("fixup", "--abort").returncode, 0)
        self.assertEqual(self.repo.git("for-each-ref"), before)
        self.assertEqual(self.repo.status_fields()["fixup"], "none")

    def test_you_can_keep_working_while_a_fixup_is_paused(self) -> None:
        """A repository mid-rebase is unusable. This one is not."""
        self.repo.review("fixup", self.target)
        self.assertEqual(self.repo.review("write").returncode, 0)
        self.repo.write("g.txt", "more work\n")
        self.repo.commit_all("carried on")
        self.repo.review("review")
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")

    def test_continue_finishes_after_both_resolutions(self) -> None:
        self.repo.review("fixup", self.target)
        for _ in range(2):
            if self.repo.status_fields()["fixup"] != "pending":
                break
            self.repo.review("resolve")
            self.repo.review("fixup", "--continue")
        self.assertEqual(self.repo.status_fields()["fixup"], "none")
        self.assertEqual(self.repo.log(self.clean), ["B", "A", "base"])


# ---------------------------------------------------------------------------
# Sessions: one review per branch
# ---------------------------------------------------------------------------

class Sessions(ReviewTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("work on wip")
        self.repo.git("branch", "second", self.base)

    def test_two_branches_keep_separate_reviews(self) -> None:
        self.start(self.base)
        self.repo.review("review")
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.review("write")
        self.repo.git("switch", "-q", "second")
        self.repo.write("g.txt", "other work\n")
        self.repo.commit_all("work on second")
        self.repo.review("start", "--from", self.base)
        self.repo.review("review")
        self.assertEqual(self.repo.status_fields()["outstanding"], "1")
        self.repo.review("write")
        self.repo.git("switch", "-q", "wip")
        self.repo.review("review")
        self.assertEqual(self.repo.index_tree(), approved, "the first review was lost")

    def test_list_shows_every_session_and_drop_removes_one(self) -> None:
        self.start(self.base)
        listed = self.repo.review("list")
        self.assertIn("wip", listed.stdout)
        self.assertEqual(self.repo.review("drop", "wip").returncode, 0)
        self.assertNotIn("wip", self.repo.review("list").stdout)

    def test_a_branch_name_with_slashes_works(self) -> None:
        """refs/review/agent/foo and refs/review/agent cannot both exist."""
        self.repo.git("switch", "-q", "-c", "agent/foo", self.base)
        self.repo.write("h.txt", "x\n")
        self.repo.commit_all("work")
        self.assertEqual(self.repo.review("start", "--from", self.base).returncode, 0)
        self.repo.git("switch", "-q", "-c", "agent", self.base)
        self.repo.write("i.txt", "y\n")
        self.repo.commit_all("work")
        self.assertEqual(self.repo.review("start", "--from", self.base).returncode, 0)

    def test_an_index_file_is_only_a_cache(self) -> None:
        """Deleting it must cost a re-stat, not the approvals."""
        self.start(self.base)
        self.repo.review("review")
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.repo.review("write")
        for stale in (self.repo.path / ".git" / "review").rglob("index.review"):
            stale.unlink()
        self.repo.review("review")
        self.assertEqual(self.repo.index_tree(), approved)


# ---------------------------------------------------------------------------
# Guardrails
# ---------------------------------------------------------------------------

class Guardrails(ReviewTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.base = self.make_base()
        self.repo.git("branch", "elsewhere", self.base)
        self.repo.write("f.txt", "ONE\ntwo\nthree\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.review("review")

    def approve_everything(self) -> None:
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "Everything")

    def test_an_unfinished_review_blocks_a_checkout_by_itself(self) -> None:
        """Git's own protection, which exists only while something is unreviewed."""
        result = subprocess.run(["git", "-C", str(self.repo.path), "switch", "elsewhere"],
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)

    def test_a_finished_review_does_not_and_that_is_the_hazard(self) -> None:
        self.approve_everything()
        result = subprocess.run(["git", "-C", str(self.repo.path), "switch", "elsewhere"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, "git no longer protects us here")

    def test_being_moved_out_of_review_mode_is_detected(self) -> None:
        self.approve_everything()
        self.repo.git("switch", "-q", "elsewhere")
        self.assertEqual(self.repo.status_fields()["consistent"], "no")

    def test_the_tool_refuses_to_clobber_an_index_after_being_moved(self) -> None:
        """The whole bug: the checkout is survivable, the blind mode switch is not."""
        self.approve_everything()
        approved = self.repo.index_tree()
        self.repo.git("switch", "-q", "elsewhere")
        self.assertEqual(self.repo.review("write").returncode, 1)
        self.repo.git("switch", "-q", "wip")
        self.repo.review("review")
        self.assertEqual(self.repo.index_tree(), approved, "approvals were destroyed")

    def test_approvals_are_recorded_as_they_are_made(self) -> None:
        """post-index-change, so nothing is lost between invocations."""
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        clean = self.repo.status_fields()["clean"].rsplit("/", 1)[-1]
        recorded = self.repo.git("rev-parse", f"refs/review/{clean}^{{tree}}", check=False)
        self.assertEqual(recorded, approved)

    def test_recovery_restores_the_last_recorded_approvals(self) -> None:
        self.repo.git("add", "f.txt")
        approved = self.repo.index_tree()
        self.approve_everything()
        self.repo.git("switch", "-q", "elsewhere")
        self.repo.git("switch", "-q", "wip")
        result = self.repo.review("review")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(self.repo.index_tree(), (approved, self.repo.tree_of("HEAD")))

    def test_status_is_safe_to_run_in_a_broken_state(self) -> None:
        self.approve_everything()
        self.repo.git("switch", "-q", "elsewhere")
        self.assertEqual(self.repo.review("status", "--porcelain").returncode, 0)


if __name__ == "__main__":
    unittest.main()
