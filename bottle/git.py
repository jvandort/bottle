import subprocess
from pathlib import Path

from bottle.errors import BottleError


def git(*args: str | Path, repo: Path | None = None) -> str:
    """Run git and return its stripped stdout, raising BottleError on failure."""
    cmd = _command(args, repo)
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        raise BottleError(result.stderr.strip() or f"{' '.join(cmd)} failed")
    return result.stdout.strip()


def _command(args, repo: Path | None) -> list[str]:
    cmd = ["git"]
    if repo is not None:
        cmd += ["-C", str(repo)]
    return cmd + [str(a) for a in args]
