"""Folding approved changes into an existing clean commit."""

from .support import CurateTestCase


class Fixup(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.repo.write("seed.txt", "seed\n")
        self.base = self.repo.commit_all("base")
        self.repo.write("a.txt", "A\nA more\n")
        self.repo.write("b.txt", "B\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")
        self.repo.stage_hunk("a.txt", "A\nA more\n", "A\n")   # approve part of a.txt
        self.repo.git("commit", "-q", "-m", "Add feature A")
        self.repo.git("add", "b.txt")
        self.repo.git("commit", "-q", "-m", "Add feature B")
        self.clean = self.repo.status_fields()["clean"]
        self.target = self.repo.git("rev-parse", f"{self.clean}~1")

    def test_absorbs_rather_than_appends(self) -> None:
        self.repo.git("add", "a.txt")
        self.assertEqual(self.repo.curate("fixup", self.target).returncode, 0)
        self.assertEqual(self.repo.log(self.clean), ["Add feature B", "Add feature A", "base"])
        self.assertEqual(self.repo.file_at(f"{self.clean}~1", "a.txt"), "A\nA more\n")

    def test_never_touches_the_working_tree(self) -> None:
        """No checkout, no stash: the whole point of doing it in the object database."""
        before = self.repo.mtimes()
        self.repo.git("add", "a.txt")
        self.repo.curate("fixup", self.target)
        self.assertEqual(self.repo.mtimes(), before)

    def test_preserves_authorship_of_replayed_commits(self) -> None:
        authors_before = self.repo.git("log", "--format=%an <%ae> %aI", self.clean)
        self.repo.git("add", "a.txt")
        self.repo.curate("fixup", self.target)
        self.assertEqual(self.repo.git("log", "--format=%an <%ae> %aI", self.clean),
                         authors_before)

    def test_amending_the_tip_needs_no_replay(self) -> None:
        self.repo.git("add", "a.txt")
        tip = self.repo.git("rev-parse", self.clean)
        self.assertEqual(self.repo.curate("fixup", tip).returncode, 0)
        self.assertEqual(len(self.repo.log(self.clean)), 3)

    def test_refuses_a_target_that_is_not_on_the_clean_line(self) -> None:
        self.repo.git("add", "a.txt")
        stranger = self.repo.git("rev-parse", self.repo.status_fields()["working"])
        self.assertEqual(self.repo.curate("fixup", stranger).returncode, 1)

    def test_refuses_when_nothing_is_approved_to_fix_up(self) -> None:
        result = self.repo.curate("fixup", self.target)
        self.assertEqual(result.returncode, 1)
        self.assertIn("nothing", (result.stdout + result.stderr).lower())

    def test_refuses_outside_review_mode(self) -> None:
        self.repo.curate("write")
        self.assertEqual(self.repo.curate("fixup", self.target).returncode, 1)

    def test_handles_a_root_commit_as_the_target(self) -> None:
        self.repo.git("add", "a.txt")
        root = self.repo.git("rev-list", "--max-parents=0", self.clean)
        result = self.repo.curate("fixup", root)
        self.assertIn(result.returncode, (0, 2), result.stderr)
        self.assertEqual(self.repo.git("rev-list", "--count", "--max-parents=0", self.clean), "1")


class FixupConflicts(CurateTestCase):
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
        self.repo.curate("review")
        self.repo.stage_hunk("f.txt", "four\n", "two\n")
        self.repo.git("commit", "-q", "-m", "A")
        self.repo.stage_hunk("f.txt", "four\n", "three\n")
        self.repo.git("commit", "-q", "-m", "B")
        self.clean = self.repo.status_fields()["clean"]
        self.target = self.repo.git("rev-parse", f"{self.clean}~1")
        self.repo.git("add", "f.txt")
        self.repo.use_merge_tool()

    def test_reports_a_conflict_without_moving_anything(self) -> None:
        before = self.repo.git("for-each-ref")
        tree_before = self.repo.mtimes()
        result = self.repo.curate("fixup", self.target)
        self.assertEqual(result.returncode, 2)
        self.assertIn("f.txt", result.stdout + result.stderr)
        self.assertEqual(self.repo.git("for-each-ref"), before, "a ref moved on conflict")
        self.assertEqual(self.repo.mtimes(), tree_before, "the working tree was touched")

    def test_warns_that_a_later_commit_will_conflict_too(self) -> None:
        result = self.repo.curate("fixup", self.target)
        self.assertIn("B", result.stdout + result.stderr,
                      "should name the later commit that also changes f.txt")

    def test_status_says_a_fixup_is_pending(self) -> None:
        self.repo.curate("fixup", self.target)
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")

    def test_resolve_offers_all_three_stages(self) -> None:
        self.repo.curate("fixup", self.target)
        self.repo.curate("resolve")
        scratch = self.repo.path / ".git" / "curate"
        names = {p.name for p in scratch.rglob("f.txt.*")}
        self.assertEqual(names, {"f.txt.BASE", "f.txt.LOCAL", "f.txt.REMOTE"})

    def test_abort_leaves_every_ref_exactly_where_it_was(self) -> None:
        before = self.repo.git("for-each-ref")
        self.repo.curate("fixup", self.target)
        self.assertEqual(self.repo.curate("fixup", "--abort").returncode, 0)
        self.assertEqual(self.repo.git("for-each-ref"), before)
        self.assertEqual(self.repo.status_fields()["fixup"], "none")

    def test_you_can_keep_working_while_a_fixup_is_paused(self) -> None:
        """A repository mid-rebase is unusable. This one is not."""
        self.repo.curate("fixup", self.target)
        self.assertEqual(self.repo.curate("write").returncode, 0)
        self.repo.write("g.txt", "more work\n")
        self.repo.commit_all("carried on")
        self.repo.curate("review")
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")

    def test_continue_finishes_after_both_resolutions(self) -> None:
        self.repo.curate("fixup", self.target)
        for _ in range(2):
            if self.repo.status_fields()["fixup"] != "pending":
                break
            self.repo.curate("resolve")
            self.repo.curate("fixup", "--continue")
        self.assertEqual(self.repo.status_fields()["fixup"], "none")
        self.assertEqual(self.repo.log(self.clean), ["B", "A", "base"])
        for commit in (f"{self.clean}~1", self.clean):
            self.assertNotIn("<<<<<<<", self.repo.file_at(commit, "f.txt"))

    def test_continue_refuses_while_a_conflict_is_unresolved(self) -> None:
        """The clean line is the one branch that exists to have no markers in it.

        Nothing resolved and `--continue` anyway used to rebuild the commit
        around merge-tree's marked-up tree and report success, which put
        markers -- nested ones, by the second step -- on the clean line.
        """
        self.repo.curate("fixup", self.target)
        before = self.repo.git("for-each-ref")
        result = self.repo.curate("fixup", "--continue")
        self.assertEqual(result.returncode, 1)
        self.assertIn("f.txt", result.stderr)
        self.assertEqual(self.repo.git("for-each-ref"), before)
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")

    def test_nor_when_the_merge_tool_left_the_markers_in(self) -> None:
        """Resolving is about what the file says, not about whether it changed."""
        self.repo.curate("fixup", self.target)
        self.repo.use_merge_tool('printf "x\n" >> "$MERGED"')
        self.repo.curate("resolve")
        self.assertEqual(self.repo.curate("fixup", "--continue").returncode, 1)

    def test_a_hand_edited_stage_file_is_a_resolution(self) -> None:
        """No merge tool is the common case at a terminal, and it has to work.

        `resolve` writes the marked-up file and says where; editing it there is
        the resolution, and nothing else has to be run to say so.
        """
        self.repo.git("config", "--unset", "merge.tool")
        self.repo.curate("fixup", self.target)
        self.repo.curate("resolve")
        self.repo.stage_file("f.txt").write_text("by hand\n")
        self.assertEqual(self.repo.curate("fixup", "--continue").returncode, 2)
        self.repo.stage_file("f.txt").write_text("by hand\n")   # again, on replay
        self.assertEqual(self.repo.curate("fixup", "--continue").returncode, 0)
        self.assertEqual(self.repo.file_at(f"{self.clean}~1", "f.txt"), "by hand\n")

    def test_resolve_does_not_clobber_a_hand_edit(self) -> None:
        """The stage files are where a resolution lives; there is nowhere else."""
        self.repo.git("config", "--unset", "merge.tool")
        self.repo.curate("fixup", self.target)
        self.repo.curate("resolve")
        self.repo.stage_file("f.txt").write_text("by hand\n")
        self.repo.curate("resolve")
        self.assertEqual(self.repo.stage_file("f.txt").read_text(), "by hand\n")


class TwoAtOnce(CurateTestCase):
    """Two reviews, both paused on a fixup conflict, neither in the other's way."""

    def paused_fixup_on(self, branch: str, base: str) -> str:
        """Leave `branch` with a review whose fixup is stuck on a conflict."""
        self.repo.git("switch", "-q", "-c", branch, base)
        self.repo.write("f.txt", f"{branch} four\n")
        self.repo.commit_all(f"{branch} work")
        self.assertEqual(self.repo.curate("start", "--from", base).returncode, 0)
        self.repo.curate("review")
        self.repo.stage_hunk("f.txt", "", f"{branch} two\n")
        self.repo.git("commit", "-q", "-m", f"{branch} A")
        self.repo.stage_hunk("f.txt", "", f"{branch} three\n")
        self.repo.git("commit", "-q", "-m", f"{branch} B")
        clean = self.repo.status_fields()["clean"]
        self.repo.use_merge_tool()
        self.repo.git("add", "f.txt")
        target = self.repo.git("rev-parse", f"{clean}~1")
        self.assertEqual(self.repo.curate("fixup", target).returncode, 2)
        return clean

    def test_each_review_keeps_its_own_paused_fixup(self) -> None:
        base = self.make_base()
        first = self.paused_fixup_on("one", base)
        self.repo.curate("write")

        second = self.paused_fixup_on("two", base)
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")
        self.repo.curate("write")

        # Back to the first, which is exactly where it was left.
        self.repo.git("switch", "-q", "one")
        self.repo.curate("review")
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")
        self.assertEqual(self.repo.status_fields()["clean"], first)

        # And finishing one does not disturb the other.
        for _ in range(3):
            if self.repo.status_fields()["fixup"] != "pending":
                break
            self.repo.curate("resolve")
            self.repo.curate("fixup", "--continue")
        self.assertEqual(self.repo.status_fields()["fixup"], "none")
        self.repo.curate("write")
        self.repo.git("switch", "-q", "two")
        self.repo.curate("review")
        self.assertEqual(self.repo.status_fields()["clean"], second)
        self.assertEqual(self.repo.status_fields()["fixup"], "pending")


class Bytes(CurateTestCase):
    """What `resolve` hands a merge tool is the file, exactly.

    It used to come through `git()`, whose output is stripped, so the first
    line lost its indentation and trailing blank lines went away -- and then
    whatever the tool produced from that was hashed straight onto the clean
    line. Content is bytes the whole way.
    """

    CONTENT = "    def f():\n        return 1\n\n\n"

    def setUp(self) -> None:
        super().setUp()
        self.repo.write("f.txt", self.CONTENT)
        self.base = self.repo.commit_all("base")
        self.repo.write("f.txt", self.CONTENT.replace("1", "4"))
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")
        self.repo.stage_hunk("f.txt", "", self.CONTENT.replace("1", "2"))
        self.repo.git("commit", "-q", "-m", "A")
        self.repo.stage_hunk("f.txt", "", self.CONTENT.replace("1", "3"))
        self.repo.git("commit", "-q", "-m", "B")
        self.clean = self.repo.status_fields()["clean"]
        self.target = self.repo.git("rev-parse", f"{self.clean}~1")
        self.repo.git("add", "f.txt")

    def test_the_sides_are_the_blobs_verbatim(self) -> None:
        self.repo.curate("fixup", self.target)
        local = self.repo.stage_file("f.txt").with_suffix(".txt.LOCAL")
        self.assertEqual(local.read_text(), self.CONTENT.replace("1", "2"))

    def test_a_resolution_lands_byte_for_byte(self) -> None:
        self.repo.use_merge_tool()          # takes LOCAL wholesale
        self.repo.curate("fixup", self.target)
        for _ in range(2):
            if self.repo.status_fields()["fixup"] != "pending":
                break
            self.repo.curate("resolve")
            self.repo.curate("fixup", "--continue")
        self.assertEqual(self.repo.status_fields()["fixup"], "none")
        self.assertEqual(self.repo.file_at(f"{self.clean}~1", "f.txt"),
                         self.CONTENT.replace("1", "2"))


class BinaryConflict(CurateTestCase):
    """merge-tree writes one side for a binary conflict and calls it a conflict.

    There are no markers to remove, so `--continue`'s usual test says nothing.
    Leaving the file alone is not a resolution there -- it is walking away with
    a side picked for you -- so the test becomes whether you changed it at all.
    """

    def setUp(self) -> None:
        super().setUp()
        (self.repo.path / "f.bin").write_bytes(b"base\0\n")
        self.base = self.repo.commit_all("base")
        (self.repo.path / "f.bin").write_bytes(b"final\0\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")
        for name in ("A", "B"):
            self.repo.git("update-index", "--add", "--cacheinfo",
                          f"100644,{self.repo.blob(name + chr(0))},f.bin")
            self.repo.git("commit", "-q", "-m", name)
        self.clean = self.repo.status_fields()["clean"]
        self.target = self.repo.git("rev-parse", f"{self.clean}~1")
        self.repo.git("add", "f.bin")

    def test_the_side_is_written_as_bytes(self) -> None:
        self.repo.curate("fixup", self.target)
        given = self.repo.stage_file("f.bin").read_bytes()
        self.assertNotIn(b"<<<<<<<", given, "merge-tree cannot mark up a binary")
        self.assertIn(b"\0", given, "and a decode would have corrupted it")

    def test_leaving_it_untouched_is_not_a_resolution(self) -> None:
        self.repo.curate("fixup", self.target)
        before = self.repo.git("for-each-ref")
        result = self.repo.curate("fixup", "--continue")
        self.assertEqual(result.returncode, 1)
        self.assertIn("f.bin", result.stderr)
        self.assertEqual(self.repo.git("for-each-ref"), before)

    def test_changing_it_is(self) -> None:
        self.repo.curate("fixup", self.target)
        for _ in range(2):
            if self.repo.status_fields()["fixup"] != "pending":
                break
            self.repo.stage_file("f.bin").write_bytes(b"chosen\0\n")
            self.repo.curate("fixup", "--continue")
        self.assertEqual(self.repo.status_fields()["fixup"], "none")
        self.assertEqual(
            self.repo.git_raw("cat-file", "blob", f"{self.clean}~1:f.bin"),
            "chosen\0\n")
