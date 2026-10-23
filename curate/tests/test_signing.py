"""Commits curate rebuilds are signed when `commit.gpgSign` says so.

`git commit` reads that setting and `git commit-tree` does not, so without
this every commit a fixup or a copy rebuilt came out unsigned.
"""

import os
from pathlib import Path

from .support import CurateTestCase

# Stands in for gpg: what git needs back to call a commit signed.
FAKE_GPG = """#!/bin/sh
cat >/dev/null
echo "[GNUPG:] SIG_CREATED " >&2
printf -- '-----BEGIN PGP SIGNATURE-----\\nfake\\n-----END PGP SIGNATURE-----\\n'
"""


class Signing(CurateTestCase):
    def setUp(self) -> None:
        super().setUp()
        gpg = Path(self.repo.path.parent / "fake-gpg")
        gpg.write_text(FAKE_GPG)
        os.chmod(gpg, 0o755)
        self.repo.git("config", "gpg.program", str(gpg))
        self.repo.git("config", "user.signingkey", "test")
        self.repo.write("a.txt", "a\n")
        self.base = self.repo.commit_all("base")
        self.repo.write("a.txt", "A\n")
        self.repo.write("b.txt", "B\n")
        self.repo.commit_all("their work")
        self.start(self.base)
        self.repo.curate("review")
        self.repo.git("add", "b.txt")
        self.repo.git("commit", "-q", "--no-gpg-sign", "-m", "Add b")
        self.repo.git("add", "a.txt")                   # to fold into "Add b"

    def signed(self, rev: str) -> bool:
        return "gpgsig " in self.repo.git("cat-file", "commit", rev)

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
