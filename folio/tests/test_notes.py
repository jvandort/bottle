import shlex
import stat
from pathlib import Path

from folio import notes, repos, snapshots, store
from tests.support import FolioTestCase, git


class NotesTest(FolioTestCase):
    """A thread's notes: a repo of its own in the thread's folio directory, at /notes in its bottle."""

    def setUp(self) -> None:
        super().setUp()
        self.ok("new", "fix-it")
        self.repo_ = repos.current(self.repo)
        self.bottle = f"{self.repo_.key}-fix-it"

    def notes(self, thread: str = "fix-it") -> Path:
        return self.repo_.directory / thread / "notes"

    def in_bottle(self) -> Path:
        """The fake bottle's checkout of /notes."""
        return Path(next(m["dir"] for m in self.fake()["bottles"][self.bottle]["mounts"] if m["path"] == "/notes"))

    def test_a_thread_starts_with_notes_of_its_own(self) -> None:
        self.assertEqual("main", git("-C", str(self.notes()), "symbolic-ref", "--short", "HEAD"))
        self.assertEqual(notes.GITIGNORE, (self.notes() / ".gitignore").read_text())
        self.assertTrue((self.notes() / ".obsidian").is_dir())
        mount = self.fake()["bottles"][self.bottle]["mounts"][1]
        self.assertEqual((mount["path"], mount["repo"], mount["remotes"]), ("/notes", str(self.notes()), notes.REMOTES))

    def test_your_copy_follows_the_agents_main(self) -> None:
        # The agent pushes main to the notes repo's lane; your main tracks it, so bottle's follow moves it.
        self.assertEqual("refs/remotes/bottle/main",
                         git("-C", str(self.notes()), "for-each-ref", "--format=%(upstream)", "refs/heads/main"))
        self.assertEqual("git --namespace=notes-out upload-pack",
                         git("-C", str(self.notes()), "config", "remote.bottle.uploadpack"))

    def test_a_pause_keeps_whats_not_pushed_and_the_resume_brings_it_back(self) -> None:
        (self.in_bottle() / "Lead.md").write_text("look at the cache\n")
        self.ok("pause", "fix-it")
        slot = store.current_slot(self.repo_.directory / "fix-it")
        self.assertTrue((slot / snapshots.NOTES_ARCHIVE).exists())
        self.ok("shell", "fix-it")
        self.assertEqual("look at the cache\n", (self.in_bottle() / "Lead.md").read_text())
        # Pointed at the new bottle's objects and remotes, wherever the old one had them.
        repointed = [c for c in self.calls("exec") if "remote set-url" in c[-1] and "/notes" in c[-1]]
        self.assertTrue(repointed)

    def test_a_resume_of_a_snapshot_from_before_notes(self) -> None:
        self.ok("pause", "fix-it")
        (store.current_slot(self.repo_.directory / "fix-it") / snapshots.NOTES_ARCHIVE).unlink()
        self.ok("shell", "fix-it")
        self.assertTrue((self.in_bottle() / ".gitignore").exists())   # checked out afresh

    def test_delete_keeps_them_and_a_new_thread_of_the_name_finds_them(self) -> None:
        git("-C", str(self.notes()), "commit", "-q", "--allow-empty", "-m", "what I learnt")
        kept = git("-C", str(self.notes()), "rev-parse", "HEAD")
        self.ok("delete", "fix-it", "--yes")
        self.assertEqual(kept, git("-C", str(self.notes()), "rev-parse", "HEAD"))
        self.ok("new", "fix-it")
        self.assertEqual(kept, git("-C", str(self.notes()), "rev-parse", "HEAD"))
        self.assertEqual(str(self.notes()), self.fake()["bottles"][self.bottle]["mounts"][1]["repo"])

    def test_a_rename_takes_them_along(self) -> None:
        self.ok("pause", "fix-it")
        self.ok("rename", "fix-it", "sort-it")
        self.assertFalse(self.notes().exists())
        self.assertEqual("main", git("-C", str(self.notes("sort-it")), "symbolic-ref", "--short", "HEAD"))

    def test_folio_notes_opens_them(self) -> None:
        opener, log = self.tmp / "open", self.tmp / "opened"
        opener.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> {shlex.quote(str(log))}\n')
        opener.chmod(opener.stat().st_mode | stat.S_IEXEC)
        self.env.update(FOLIO_OPEN=str(opener), FOLIO_OBSIDIAN_CONFIG=str(self.tmp / "obsidian.json"))
        out = self.ok("notes", "fix-it")
        self.assertIn("Opened", out)
        self.assertIn(str(self.notes().resolve()), (self.tmp / "obsidian.json").read_text())

    def test_no_notes_yet(self) -> None:
        result = self.folio("notes", "other")
        self.assertNotEqual(0, result.returncode)
        self.assertIn("has no notes yet", result.stderr)
