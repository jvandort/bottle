"""A thread's notes: a repo its agent keeps, and you read in Obsidian.

Every thread has one, at <thread>/notes in its folio directory (store.py),
made the first time it's wanted: checked out in the thread's bottle at /notes,
where the agent commits to main and pushes it to its `agent` remote, the
notes repo's own lane. Your copy's main tracks that lane (remote `bottle`),
so bottle's follow keeps the notes you open in Obsidian current.

They're the thread's for good: they move with a rename, since they're in the
thread's directory; a delete leaves them; and a new thread of the same name
finds them. The lane's namespaces are named once for all, notes-out and
notes-in, since the repo is one thread's alone.
"""

from pathlib import Path

from . import lanes
from .git import git

MOUNT = "/notes"  # where a thread's bottle has them
REMOTE = "bottle"
OUT, IN = "notes-out", "notes-in"
# The notes mount's remotes in the bottle: your main, its lane, and yours.
REMOTES = ["host:fetch=heads", f"agent:fetch=ns/{OUT},push=ns/{OUT}", f"user:fetch=ns/{IN}"]
# What Obsidian writes for whoever has the vault open: theirs, not the notes.
GITIGNORE = ".obsidian/\n.trash/\n"


def ensure(path: Path, first: str = "Notes") -> Path:
    """The notes repo at `path`, made with its first commit if it isn't there; its lane and tracking set."""
    if not (path / ".git").exists():
        path.mkdir(parents=True, exist_ok=True)
        git("init", "-q", "-b", "main", str(path))
        (path / ".gitignore").write_text(GITIGNORE)
        # Obsidian's settings for it, so Obsidian takes it for a vault it knows, not a new one (see obsidian.py).
        (path / ".obsidian").mkdir(exist_ok=True)
        git("add", ".gitignore", repo=path)
        git("-c", "user.name=folio", "-c", "user.email=folio@localhost", "commit", "-q", "-m", first, repo=path)
    lanes.register_remote(path, REMOTE, OUT, IN)
    # Config alone: the tracking branch needn't exist yet for a push to find the branch that tracks it.
    git("config", "branch.main.remote", REMOTE, repo=path)
    git("config", "branch.main.merge", "refs/heads/main", repo=path)
    return path
