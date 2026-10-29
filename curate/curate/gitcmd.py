"""Every call this tool makes to git.

Almost all of it is plumbing, whose output is a stable interface. The two
porcelain commands it relies on are the user's: `git add -p` and, in review
mode, `git commit`.
"""

import atexit
import os
import subprocess
from pathlib import Path

from .errors import Refused
from .timing import looked_up, timed

HOOK_GUARD = "CURATE_IN_HOOK"

# What merge-tree hands back for a conflicted path: stage -> (mode, oid),
# where stage 1 is the merge base, 2 is ours and 3 is theirs.
Stages = dict[int, tuple[str, str]]
Conflicts = dict[str, Stages]


def environment(index: str | Path | None = None,
                env: dict[str, str] | None = None) -> dict[str, str]:
    """The environment every git we run gets. GIT_INDEX_FILE is ours to set,
    never inherited."""
    e = dict(os.environ)
    e.pop("GIT_INDEX_FILE", None)
    e[HOOK_GUARD] = "1"          # git we run must not re-enter our own hooks
    if index is not None:
        e["GIT_INDEX_FILE"] = str(index)
    if env:
        e.update(env)
    return e


def run(args: list[str], index: str | Path | None = None,
        env: dict[str, str] | None = None,
        stdin: str | None = None) -> subprocess.CompletedProcess[str]:
    """Run git and decode its output as text."""
    with timed(["git", *args]):
        return subprocess.run(["git", *args], capture_output=True, text=True,
                              env=environment(index, env), input=stdin)


def git(*args: str, index: str | Path | None = None,
        env: dict[str, str] | None = None, check: bool = True) -> str:
    p = run(list(args), index=index, env=env)
    if check and p.returncode != 0:
        raise Refused(f"git {' '.join(args)} failed: {p.stderr.strip()}")
    return p.stdout.strip()


def git_ok(*args: str, index: str | Path | None = None,
           env: dict[str, str] | None = None) -> bool:
    return run(list(args), index=index, env=env).returncode == 0


class Objects:
    """What names resolve to, and what objects hold, from one `git cat-file
    --batch-command` kept open.

    Most of what curate asks git is a question like "where is this branch?" or
    "what does this file say on that side?", and a git apiece costs tens of
    milliseconds on macOS. This asks them all of one process instead, started
    by the first question -- so a command that asks none starts nothing -- and
    stopped when curate exits. Each answer is read as git has it at that
    moment, so what curate's own git just wrote is what the next question
    sees: there is nothing to keep fresh. Repo holds one for the command; pass
    it to whatever needs it.

    Some names git refuses with a fatal error rather than "missing" --
    `wip@{upstream}` for a branch with none -- and the process exits. That name
    resolves to nothing, as it would have for `rev-parse --verify -q`, and the
    next question starts another.
    """

    def __init__(self) -> None:
        self._process: subprocess.Popen[bytes] | None = None

    def rev(self, spec: str) -> str | None:
        """The commit (or other object) `spec` names, tags peeled; None if it names nothing."""
        found = self._ask("info", f"{spec}^{{}}")
        return found[0] if found else None

    def tree(self, spec: str) -> str | None:
        """The tree of the commit `spec` names; None if it names nothing."""
        found = self._ask("info", f"{spec}^{{tree}}")
        return found[0] if found else None

    def contents(self, spec: str) -> bytes | None:
        """An object's exact bytes -- a blob's are the file's -- or None if it is not there.

        Bytes, never text: decoding and stripping would reindent a first line
        and drop trailing blank lines (see `fixup.materialise`).
        """
        found = self._ask("contents", spec)
        return found[1] if found else None

    def subject(self, commit: str) -> str:
        """A commit's subject, as `git log --format=%s` gives it; "" if there is no such commit."""
        raw = self.contents(f"{commit}^{{commit}}")
        if raw is None:
            return ""
        message = raw.split(b"\n\n", 1)[1] if b"\n\n" in raw else b""
        return " ".join(message.lstrip(b"\n").split(b"\n\n", 1)[0].decode(errors="replace").split("\n")).strip()

    def _ask(self, command: str, name: str) -> tuple[str, bytes] | None:
        """(oid, contents) -- contents only for `contents` -- or None if `name` names nothing."""
        if not name or "\n" in name:
            return None
        process = self._started()
        assert process.stdin is not None and process.stdout is not None
        with looked_up(name if command == "info" else f"{command} {name}"):
            try:
                process.stdin.write(f"{command} {name}\n".encode())
                process.stdin.flush()
                header = process.stdout.readline()
                # "<oid> <type> <size>", or "<name> missing" / "<name> ambiguous".
                fields = header.split()
                if len(fields) != 3:
                    if not header:                   # git gave up on this name, and exited
                        self.close()
                    return None
                body = b""
                if command == "contents":
                    body = process.stdout.read(int(fields[2]))
                    process.stdout.read(1)            # the newline after the contents
            except BrokenPipeError:                  # gave up on the name before, and exited
                self.close()
                return None
        return fields[0].decode(), body

    def _started(self) -> "subprocess.Popen[bytes]":
        if self._process is None:
            with timed(["git", "cat-file", "--batch-command"]):
                self._process = subprocess.Popen(
                    ["git", "cat-file", "--batch-command"], stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=environment())
            atexit.register(self.close)
        return self._process

    def close(self) -> None:
        process, self._process = self._process, None
        if process is None:
            return
        atexit.unregister(self.close)
        try:
            if process.stdin is not None:
                process.stdin.close()
        except BrokenPipeError:
            pass
        process.wait()


class Refs:
    """The branches, remote branches and tags as they stood, read with one git.

    For working out what to do before anything is written -- a command's
    questions, its lists -- where git would otherwise be asked about one name
    at a time. Never kept past a write: it is a snapshot, and curate's own
    writes would make it wrong. So it is made where the planning starts and
    passed to what plans, and nothing that runs after a write is given it.

    It answers only what it is sure of: a name that is ambiguous (a tag and a
    branch, say), or not a branch, remote branch or tag at all, goes to git
    as it always did. So an answer from here is the one git would give.
    """

    UPSTREAM = "@{upstream}"

    def __init__(self) -> None:
        out = run(["for-each-ref", "--format=%(refname)%1f%(refname:short)%1f%(upstream)%1f%(symref)",
                   "refs/heads", "refs/remotes", "refs/tags"]).stdout
        self._short: dict[str, str] = {}
        self._upstream: dict[str, str] = {}
        self._symref: dict[str, str] = {}
        for line in out.splitlines():
            ref, short, upstream, symref = line.split("\x1f")
            self._short[ref], self._upstream[ref] = short, upstream
            if symref:
                self._symref[ref] = symref

    def full(self, name: str) -> str:
        """What `git rev-parse --symbolic-full-name <name>` says, or "" if nothing.

        `name` may end in @{upstream}. A symbolic ref (origin/HEAD) is the ref
        it points at, as git has it.
        """
        upstream = name.endswith(self.UPSTREAM)
        ref = self._ref(name[:-len(self.UPSTREAM)] if upstream else name)
        if ref is None:
            return git("rev-parse", "--symbolic-full-name", name, check=False)
        if upstream:
            return self._upstream.get(ref, "")
        return ref

    def abbrev(self, name: str) -> str:
        """What `git rev-parse --abbrev-ref <name>` says, or "" if nothing."""
        full = self.full(name)
        if full in self._short:
            return self._short[full]
        return git("rev-parse", "--abbrev-ref", name, check=False) if full else ""

    def _ref(self, name: str) -> str | None:
        """The ref `name` means, symbolic refs followed; None when only git can say."""
        if name.startswith("refs/"):
            found = [name] if name in self._short else []
        else:
            # git's order, from `git help revisions`; more than one is ambiguous.
            found = [c for c in (f"refs/{name}", f"refs/tags/{name}", f"refs/heads/{name}",
                                 f"refs/remotes/{name}", f"refs/remotes/{name}/HEAD") if c in self._short]
        if len(found) != 1:
            return None
        return self._symref.get(found[0], found[0])


def only_on(ref: str, *ignoring: str) -> list[str]:
    """The commits on `ref` that no other ref can reach, newest first.

    What is lost by deleting `ref`, give or take a reflog. `ignoring` names
    refs that go with it. Refs cannot contain glob characters, so a full ref
    name is safe as an --exclude pattern.
    """
    excludes = [f"--exclude={r}" for r in (ref, *ignoring)]
    out = git("rev-list", ref, "--not", *excludes, "--all")
    return out.split()


def parents_of(commit: str) -> list[str]:
    return git("rev-list", "--parents", "-n", "1", commit).split()[1:]


def metadata(commit: str) -> tuple[dict[str, str], str]:
    """Author identity and message of a commit, for rebuilding it faithfully.

    Forgetting this silently rewrites authorship, which nobody notices for a
    month. The committer is deliberately whoever ran the fixup.
    """
    out = git("log", "-1", "--format=%an%n%ae%n%aI%n%B", commit)
    name, email, date, message = (out.split("\n", 3) + ["", "", "", ""])[:4]
    return ({"GIT_AUTHOR_NAME": name,
             "GIT_AUTHOR_EMAIL": email,
             "GIT_AUTHOR_DATE": date}, message)


def signing() -> bool:
    """Whether commits should be signed: `commit.gpgSign`, which `git commit`
    reads and `git commit-tree` does not."""
    return git("config", "--type=bool", "--get", "commit.gpgsign",
               check=False) == "true"


def commit_tree(tree: str, parents: list[str], message: str,
                env: dict[str, str] | None = None, sign: bool = False) -> str:
    """A commit, and nothing else moved.

    `sign` is for commits people read -- a rebuilt clean commit, an adopted
    one -- which are signed when `commit.gpgSign` says so, as `git commit`
    would have. Never for curate's own bookkeeping: the approved set is
    recorded on every `git add`, and a passphrase prompt there would be absurd.
    """
    args = ["commit-tree", tree]
    for p in parents:
        args += ["-p", p]
    if sign and signing():
        args.append("-S")
    return git(*args, "-m", message, env=env)


def merge_tree(base: str, ours: str,
               theirs: str) -> tuple[str, Conflicts | None]:
    """Three-way merge in the object database. Returns (tree, conflicts).

    conflicts is None on success, otherwise {path: {stage: (mode, oid)}} and
    the tree is the one with ordinary conflict markers in it.
    """
    p = run(["merge-tree", "--write-tree", f"--merge-base={base}", ours, theirs])
    lines = p.stdout.splitlines()
    if p.returncode == 0:
        return lines[0], None
    if p.returncode != 1:
        raise Refused(f"merge-tree failed: {p.stderr.strip() or p.stdout.strip()}")
    tree: str = lines[0]
    conflicts: Conflicts = {}
    for line in lines[1:]:
        if not line.strip():
            break
        info, _, path = line.partition("\t")
        mode, oid, stage = info.split()
        conflicts.setdefault(path, {})[int(stage)] = (mode, oid)
    return tree, conflicts
