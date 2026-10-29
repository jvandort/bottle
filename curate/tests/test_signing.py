"""Commits curate rebuilds are signed when `commit.gpgSign` says so.

`git commit` reads that setting and `git commit-tree` does not, so without
this every commit a fixup or a copy rebuilt came out unsigned.
"""

import os
from pathlib import Path

from .support import CurateTestCase, Repo

# Stands in for gpg: what git needs back to call a commit signed.
FAKE_GPG = """#!/bin/sh
cat >/dev/null
echo "[GNUPG:] SIG_CREATED " >&2
printf -- '-----BEGIN PGP SIGNATURE-----\\nfake\\n-----END PGP SIGNATURE-----\\n'
"""


def fake_gpg(repo: Repo) -> None:
    """Signing that works without a key, for curate to be asked to do."""
    gpg = Path(repo.path.parent / "fake-gpg")
    gpg.write_text(FAKE_GPG)
    os.chmod(gpg, 0o755)
    repo.git("config", "gpg.program", str(gpg))
    repo.git("config", "user.signingkey", "test")


def signed(repo: Repo, rev: str) -> bool:
    return "gpgsig " in repo.git("cat-file", "commit", rev)


class Signing(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        fake_gpg(self.repo)
        self.repo.write("a.txt", "a\n")
        self.base = self.repo.commit_all("base")
        self.repo.write("a.txt", "A\n")
        self.repo.write("b.txt", "B\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("read")
        self.repo.git("add", "b.txt")
        self.repo.git("commit", "-q", "--no-gpg-sign", "-m", "Add b")
        self.repo.git("add", "a.txt")                   # to fold into "Add b"

    def signed(self, rev: str) -> bool:
        return signed(self.repo, rev)

    def test_a_rebuilt_clean_commit_is_signed_when_commits_are(self) -> None:
        self.repo.git("config", "commit.gpgsign", "true")
        result = self.repo.curate("fixup", "curate/wip")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.signed("curate/wip"))

    def test_and_not_when_they_are_not(self) -> None:
        result = self.repo.curate("fixup", "curate/wip")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.signed("curate/wip"))

    def test_the_approved_set_is_bookkeeping_and_never_signed(self) -> None:
        """It is recorded on every `git add`: a passphrase prompt each time."""
        self.repo.git("config", "commit.gpgsign", "true")
        self.repo.write("a.txt", "AA\n")
        self.repo.git("add", "a.txt")
        self.assertFalse(self.signed("refs/curate/curate%2Fwip"))

    def test_a_fold_below_the_tip_signs_it_and_what_is_replayed_onto_it(self) -> None:
        self.repo.git("config", "commit.gpgsign", "true")
        tip = self.repo.git("rev-parse", "curate/wip")
        later = self.repo.git("commit-tree", self.repo.tree_of(tip), "-p", tip,
                              "--no-gpg-sign", "-m", "later")
        self.repo.git("update-ref", "refs/heads/curate/wip", later)
        result = self.repo.curate("fixup", tip)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.repo.log("curate/wip"), ["later", "Add b", "base"])
        self.assertTrue(self.signed("curate/wip~1"), "the fold")
        self.assertTrue(self.signed("curate/wip"), "the replay")


class SignedConflicts(CurateTestCase):
    """The same, when the fold and the replay each stop on a conflict."""

    def test_both_resolutions_are_signed(self) -> None:
        fake_gpg(self.repo)
        self.repo.write("f.txt", "one\n")
        base = self.repo.commit_all("base")
        self.repo.write("f.txt", "four\n")
        self.repo.commit_all("their work")
        self.start(base)
        self.repo.curate("read")
        self.repo.stage_hunk("f.txt", "four\n", "two\n")
        self.repo.git("commit", "-q", "--no-gpg-sign", "-m", "A")
        self.repo.stage_hunk("f.txt", "four\n", "three\n")
        self.repo.git("commit", "-q", "--no-gpg-sign", "-m", "B")
        self.repo.git("add", "f.txt")
        self.repo.git("config", "commit.gpgsign", "true")
        self.repo.curate("fixup", "curate/wip~1")
        for code in (2, 0):                     # the fold, then the replay
            self.repo.curate("resolve")
            self.repo.stage_file("f.txt").write_text("by hand\n")
            self.assertEqual(self.repo.curate("fixup", "--continue").returncode, code)
        self.assertEqual(self.repo.log("curate/wip"), ["B", "A", "base"])
        self.assertTrue(signed(self.repo, "curate/wip~1"), "the fold")
        self.assertTrue(signed(self.repo, "curate/wip"), "the replay")
