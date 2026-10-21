import subprocess
from pathlib import Path

from .errors import FolioError


def git(*args: str, repo: Path | None = None, input: str | None = None, check: bool = True) -> str:
    """Run git (in `repo`, if given) and return its stripped stdout."""
    cmd = ["git", *(["-C", str(repo)] if repo else []), *args]
    result = subprocess.run(cmd, capture_output=True, text=True, input=input)
    if check and result.returncode != 0:
        raise FolioError(f"git {' '.join(args)}: {result.stderr.strip() or f'exit {result.returncode}'}")
    return result.stdout.strip()


def succeeds(*args: str, repo: Path | None = None) -> bool:
    cmd = ["git", *(["-C", str(repo)] if repo else []), *args]
    return subprocess.run(cmd, capture_output=True).returncode == 0
