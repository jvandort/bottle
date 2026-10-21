"""Snapshots: what a paused thread keeps of its bottle, in two archives.

`workspace.tar.gz` is /workspace: its `.git` (config, HEAD, the index, every
ref, the stash, the reflog, and the objects the bottle made itself -- the
ones it borrows from the repo through alternates aren't copied) and every
tracked and untracked file that isn't ignored. Ignored files (build outputs,
node_modules) are left out: they can be huge, and rebuilding them is cheaper
than carrying them. The new bottle mounts the same repo at the same path, so
the alternates file still points at the objects it borrows.

`claude.tar.gz` is ~/.claude, less what's only cache: the conversation, its
subagents and memory, /rewind's file history, the prompt history, and
settings.json, with the choices made in the thread (`/model` and the like).
settings.json comes back whole, over the one the claude feature wrote into the
new image, so a change to that feature's options since reaches a thread only
when it's started afresh; the feature rarely changes, and threads don't live
long. Leaving out by name means a new kind of Claude Code state is
kept without anyone having to notice it.

Restoring both gets back the exact workspace and conversation; a pause loses
only running processes. Archives travel over `bottle exec`, never a mount.
The scripts are POSIX sh, and `tar` either GNU's or BSD's.

An archive is the agent's, made in a bottle it controls, so on this machine
it's only ever copied, and read through to check it's whole: never unpacked,
and never run git in. A repo's own config can make git run commands
(`core.fsmonitor`, a filter driver), so git run on the agent's `.git` here
would be running the agent's commands outside the sandbox. What folio needs
to know about a paused thread's workspace -- what in it was never pushed --
it records in the bottle at pause, as `work.txt`.
"""

import tarfile
from pathlib import Path

from .errors import FolioError

WORKSPACE_ARCHIVE = "workspace.tar.gz"
CLAUDE_ARCHIVE = "claude.tar.gz"
WORK = "work.txt"  # the workspace's branches and whether it had changes, as the bottle saw them at pause

# ~/.claude's files that are only cache, and CLAUDE.md: a link to the
# ~/BOTTLE.md that every bottle writes afresh.
CLAUDE_LEFT_OUT = ("CLAUDE.md", "plugins", "cache", "telemetry", "policy-limits*", "remote-settings.json", "backups")


def workspace_out(workspace: str) -> str:
    """Writes /workspace's archive to stdout. Tracked files since deleted are skipped."""
    return (
        f"cd {workspace} && {{ printf '.git\\0'; git ls-files -z -co --exclude-standard | "
        "xargs -0 sh -c 'for f do if [ -e \"$f\" ] || [ -L \"$f\" ]; then printf \"%s\\0\" \"$f\"; fi; done' sh; } "
        "| tar -czf - --null -T -"
    )


def workspace_in(workspace: str) -> str:
    """Replaces /workspace with the archive on stdin."""
    return f"cd {workspace} && find . -mindepth 1 -maxdepth 1 -exec rm -rf {{}} + && tar -xzf -"


CLAUDE_OUT = ('mkdir -p "$HOME/.claude" && cd "$HOME/.claude" && tar -czf - '
              + " ".join(f"--exclude='./{name}'" for name in CLAUDE_LEFT_OUT) + " .")
CLAUDE_IN = 'mkdir -p "$HOME/.claude" && cd "$HOME/.claude" && tar -xzf -'


def check(path: Path) -> None:
    """Fail unless `path` is a whole gzipped tar: read to the end, every member."""
    try:
        with tarfile.open(path, "r:gz") as archive:
            for _ in archive:
                pass
    except (tarfile.TarError, OSError, EOFError) as e:
        raise FolioError(f"{path.name} came back incomplete ({e}); nothing was changed") from None

