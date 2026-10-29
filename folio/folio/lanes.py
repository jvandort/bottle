"""Lanes: how a thread's commits travel between its bottle and the user's repo.

A thread's lane is two git namespaces in the user's repo: `<thread>-out`,
which the agent writes and the user reads, and `<thread>-in`, the reverse.
Each has one writer, so neither side's push is ever refused for what the
other did, and conflicts are always the agent's to merge.

The bottle is served them as two remotes, each fetching what it pushes, so
git behaves as git: `agent`, the out lane, which it pushes to and sees its own
pushes in; and `user`, the in lane, read-only, where what the user hands over
arrives. The user's repo gets them as one remote, `bottle/<thread>`, which
folio owns: fetching reads the agent's lane, pushing hands over into the
user's. So a thread's branches can be fetched whether or not it has a bottle
right now.
"""

from pathlib import Path

from .git import git, succeeds

OUT, IN = "out", "in"
REMOTE_PREFIX = "bottle/"


def namespace(thread: str, direction: str) -> str:
    return f"{thread}-{direction}"


def remote_name(thread: str) -> str:
    return REMOTE_PREFIX + thread


def bottle_remotes(thread: str) -> list[str]:
    """The remotes a thread's bottle is served, as `bottle new --git-remote` takes them."""
    return [
        "origin:fetch=upstream",
        "host:fetch=heads",
        f"agent:fetch=ns/{namespace(thread, OUT)},push=ns/{namespace(thread, OUT)}",
        f"user:fetch=ns/{namespace(thread, IN)}",
    ]


def register(repo: Path, thread: str) -> None:
    """Make the lane remote `bottle/<thread>` of the repo: fetch the out lane, push to the in lane.

    One remote, two lanes. `uploadpack` and `receivepack` name the command git
    runs on the remote side, so pointing them at different namespaces means
    `git fetch` reads what the bottle wrote and `git push` writes what it
    reads. Every setting is written with --replace-all, so registering again
    repairs a remote that was edited.
    """
    register_remote(repo, remote_name(thread), namespace(thread, OUT), namespace(thread, IN))


def register_remote(repo: Path, name: str, out: str, in_: str) -> None:
    """Make `name` a lane remote of the repo: fetch reads namespace `out`, push writes `in_` (see register)."""
    settings = {
        "url": str(repo),
        "fetch": f"+refs/heads/*:refs/remotes/{name}/*",
        "uploadpack": f"git --namespace={out} upload-pack",
        # The user's own lane, so no restrictions: rewriting what they haven't
        # handed over yet is theirs to do.
        "receivepack": f"git -c receive.denyCurrentBranch=ignore --namespace={in_} receive-pack",
    }
    for key, value in settings.items():
        git("config", "--replace-all", f"remote.{name}.{key}", value, repo=repo)


def forget(repo: Path, thread: str) -> None:
    """Remove the lane: both namespaces' refs, and the remote with what it fetched. Gone already is fine."""
    refs = branches(repo, thread, OUT, full=True) | branches(repo, thread, IN, full=True)
    if refs:
        git("update-ref", "--stdin", repo=repo, input="".join(f"delete {ref}\n" for ref in sorted(refs)))
    if succeeds("config", "--get", f"remote.{remote_name(thread)}.url", repo=repo):
        git("remote", "remove", remote_name(thread), repo=repo)


def move_namespace(repo: Path, old: str, new: str) -> None:
    """Move every ref of namespace `old` to namespace `new`, in one update-ref transaction."""
    refs = git("for-each-ref", "--format=%(objectname) %(refname)", f"refs/namespaces/{old}/", repo=repo)
    moves = []
    for line in refs.splitlines():
        commit, ref = line.split(" ", 1)
        moved = f"refs/namespaces/{new}/" + ref.removeprefix(f"refs/namespaces/{old}/")
        moves += [f"create {moved} {commit}\n", f"delete {ref} {commit}\n"]
    if moves:
        git("update-ref", "--stdin", repo=repo, input="start\n" + "".join(moves) + "commit\n")


def rename(repo: Path, old: str, new: str) -> None:
    """Move the lane: both namespaces' refs to the new thread's, and the remote to `bottle/<new>`.

    One update-ref transaction moves every lane ref, so a failure moves none.
    `git remote rename` takes the remote's fetched branches along, and the
    upstream of every branch of yours that tracked one; registering again then
    points its uploadpack and receivepack at the new namespaces.
    """
    moves = []
    for direction in (OUT, IN):
        old_prefix, new_prefix = f"refs/namespaces/{namespace(old, direction)}/", f"refs/namespaces/{namespace(new, direction)}/"
        for ref, commit in branches(repo, old, direction, full=True).items():
            moves += [f"create {new_prefix}{ref.removeprefix(old_prefix)} {commit}\n", f"delete {ref} {commit}\n"]
    if moves:
        git("update-ref", "--stdin", repo=repo, input="start\n" + "".join(moves) + "commit\n")
    if succeeds("config", "--get", f"remote.{remote_name(old)}.url", repo=repo):
        git("remote", "rename", remote_name(old), remote_name(new), repo=repo)
    register(repo, new)


def branches(repo: Path, thread: str, direction: str = OUT, full: bool = False) -> dict[str, str]:
    """The lane's refs: branch name (or, with `full`, the whole ref) to commit."""
    prefix = f"refs/namespaces/{namespace(thread, direction)}/"
    out = git("for-each-ref", "--format=%(objectname) %(refname)", prefix, repo=repo)
    refs = {}
    for line in out.splitlines():
        commit, ref = line.split(" ", 1)
        refs[ref if full else ref.removeprefix(prefix + "refs/heads/")] = commit
    return refs


def unintegrated(repo: Path, thread: str) -> list[str]:
    """The out lane's branches with commits that aren't on the user's branches or origin's.

    Reachability, not existence: the lane's objects are in the repo either
    way, and go at its next gc once the lane is deleted.
    """
    return sorted(branch for branch, commit in branches(repo, thread).items() if not _integrated(repo, commit))


def unsaved(repo: Path, thread: str, tips: dict[str, str]) -> list[str]:
    """Of a bottle's own branch tips (name to commit), those with commits the lane and the user don't have."""
    lane = sorted(set(branches(repo, thread).values()))
    return sorted(name for name, commit in tips.items()
                  if not succeeds("cat-file", "-e", f"{commit}^{{commit}}", repo=repo)
                  or git("rev-list", "-n1", commit, "--not", "--branches", "--remotes=origin", *lane, repo=repo))


def _integrated(repo: Path, commit: str) -> bool:
    return not git("rev-list", "-n1", commit, "--not", "--branches", "--remotes=origin", repo=repo)
