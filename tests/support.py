import contextlib
import os
import shutil
import ssl
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


class GitTestCase(unittest.TestCase):
    """Runs each test in a temp dir with BOTTLE_HOME and git config isolated.

    The user's global git config is ignored so tests never sign commits,
    prompt for passphrases, or pick up hooks and aliases.
    """

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        # Resolve so paths match what git reports (/var is a symlink on macOS).
        self.tmp = Path(tmp.name).resolve()
        self.bottle_home = self.tmp / "bottle-home"
        env = mock.patch.dict(
            os.environ,
            {
                "BOTTLE_HOME": str(self.bottle_home),
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_AUTHOR_NAME": "Test",
                "GIT_AUTHOR_EMAIL": "test@example.com",
                "GIT_COMMITTER_NAME": "Test",
                "GIT_COMMITTER_EMAIL": "test@example.com",
            },
        )
        env.start()
        self.addCleanup(env.stop)

    def make_repo(self, name: str = "project", origin: str | None = None) -> Path:
        repo = self.tmp / name
        run("git", "init", "-q", "-b", "main", repo)
        self.commit(repo, "initial")
        if origin:
            run("git", "-C", repo, "remote", "add", "origin", origin)
        return repo

    def commit(self, repo: Path, message: str) -> str:
        run("git", "-C", repo, "commit", "-q", "--allow-empty", "-m", message)
        return run("git", "-C", repo, "rev-parse", "HEAD")


def run(*args: str | Path) -> str:
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True, check=True)
    return result.stdout.strip()


def client_hello(name: str | None) -> bytes:
    """The first record a TLS client sends, as a bottle's would arrive at the decider."""
    incoming, outgoing = ssl.MemoryBIO(), ssl.MemoryBIO()
    context = ssl.create_default_context()
    if name is None:
        context.check_hostname = False
    tls = context.wrap_bio(incoming, outgoing, server_hostname=name)
    with contextlib.suppress(ssl.SSLWantReadError):
        tls.do_handshake()
    return outgoing.read()


def smolvm_present() -> bool:
    """Whether the smolvm bottle would run ($BOTTLE_SMOLVM, else `smolvm` on PATH) is on this machine."""
    from bottle import smolvm

    return shutil.which(smolvm.binary()) is not None


# For tests that run real machines: they need smolvm, so they run on a Mac with it, and skip elsewhere.
requires_smolvm = unittest.skipUnless(smolvm_present(), "needs smolvm ($BOTTLE_SMOLVM, or smolvm on PATH)")
