import os
import platform
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
                "FOLIO_HOME": str(self.tmp / "folio-home"),
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


def krun_present() -> bool:
    """Whether bottle-cli ($BOTTLE_CLI, else the one `cargo xtask dist` built here) is here, on a Mac to run VMs."""
    from folio.bottle import krun

    return platform.system() == "Darwin" and os.path.isfile(krun.binary())


# For tests that run real machines: they need bottle-cli and a Mac, and skip elsewhere.
requires_krun = unittest.skipUnless(krun_present(), "needs bottle-cli on a Mac (cargo xtask dist, or $BOTTLE_CLI)")


_BUILT_CLI: list[Path] = []


def built_cli() -> Path:
    """bottle-cli, built from this repo by cargo (debug), for tests that drive the real one; skips the test where
    cargo can't build it."""
    import shutil

    if _BUILT_CLI:
        return _BUILT_CLI[0]
    repo = Path(__file__).resolve().parents[3]
    cargo = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / "cargo")
    if not Path(cargo).exists():
        raise unittest.SkipTest("needs cargo, to build bottle-cli")
    built = subprocess.run([cargo, "build", "-q", "-p", "bottle-cli"], cwd=repo, capture_output=True, text=True)
    if built.returncode != 0:
        raise unittest.SkipTest(f"bottle-cli didn't build: {built.stderr.strip()[-300:]}")
    target = Path(os.environ.get("CARGO_TARGET_DIR") or repo / "target")
    _BUILT_CLI.append(target / "debug" / "bottle-cli")
    return _BUILT_CLI[0]


def write_bottles(bottles_: dict) -> None:
    """Every bottle's record, replaced by `bottles_`'s ({name: Bottle}), in thread_dir()."""
    from folio.bottle import bottles

    for path in bottles.records().values():
        path.unlink()
    for bottle in bottles_.values():
        bottles._save(bottle, thread_dir())


def register_runner(name: str, destination: str) -> None:
    """Runner `name` added, at ssh `destination`, as `bottle runner add` records it (unchecked)."""
    from folio.bottle.store import bottle_home, read_json, write_json

    path = bottle_home() / "runners.json"
    known = (read_json(path) or {}).get("runners", {})
    write_json(path, {"version": 1, "runners": {**known, name: {"ssh": destination}}})


def thread_dir() -> "Path":
    """A thread's directory in folio's home, for the bottles a test makes: where their records are kept."""
    return Path(os.environ["FOLIO_HOME"]) / "repo-0" / "thread"


def stop_bottle(name: str) -> None:
    """Stop a bottle's machine, as bottle-cli's stop does, and folio serve's answering it: what a test stops one
    with, to start it again."""
    from folio.bottle import bottles, daemon

    bottle = bottles.get(name)
    runner = bottles.runner_of(bottle)
    if runner.state(bottle.machine) == "running":
        runner.stop(bottle.machine)
    daemon.release_egress(bottle.name)


def manifest(dist: "Path") -> None:
    """A distribution's manifest, as xtask writes it: each file's sha256 and name, as sha256sum writes them."""
    import hashlib

    names = sorted(p.name for p in dist.iterdir() if p.is_file() and not p.name.startswith("."))
    (dist / ".files").write_text("".join(f"{hashlib.sha256((dist / n).read_bytes()).hexdigest()}  {n}\n"
                                         for n in names))
