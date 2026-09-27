"""Bottles: a wrapped repo checked out in an image, running in its own VM.

The registry at $BOTTLE_HOME/bottles.json is written before anything is
created, so a failure at any point leaves a record of what may exist.
Creation rolls back on failure; delete() removes whatever parts exist, so it
also cleans up a bottle that was left half-made.

Each bottle has:
  - a ref, refs/bottle/<id>, in the wrapped repo, so its commit can't be gc'd
  - a host-only network, bottle-<id>, reaching only the host
  - an egress proxy on that network's gateway, served by bottled
  - a container, bottle-<name>, with the repo's objects mounted read-only
  - a checkout at /workspace that borrows those objects via alternates
"""

import shlex
import subprocess
import time
import uuid
from dataclasses import asdict, dataclass, field, replace

from bottle import daemon, images, prereqs, repos, runtime
from bottle.errors import BottleError
from bottle.git import git
from bottle.store import bottle_home, read_json, write_json

OBJECTS_MOUNT = "/mnt/repo/objects"
WORKSPACE = "/workspace"
USER = "genie"
NO_PROXY = "localhost,127.0.0.1"


@dataclass(frozen=True)
class Bottle:
    name: str
    id: str
    repo: str
    image: str
    branch: str | None  # None: a detached commit
    commit: str
    created: float
    # "creating" until setup finishes, "ready" after; "broken" if cleanup failed.
    status: str = "creating"

    @property
    def checkout(self) -> str:
        """What's checked out, for display: the branch, or the detached commit."""
        return self.branch or f"({self.commit[:12]})"

    @property
    def ref(self) -> str:
        return f"refs/bottle/{self.id}"

    @property
    def network(self) -> str:
        return f"bottle-{self.id}"

    @property
    def container(self) -> str:
        return f"bottle-{self.name}"


def registry_path():
    return bottle_home() / "bottles.json"


def load() -> dict[str, Bottle]:
    data = read_json(registry_path()) or {"bottles": {}}
    return {name: Bottle(**entry) for name, entry in data["bottles"].items()}


def get(name: str) -> Bottle:
    bottles = load()
    if name not in bottles:
        known = ", ".join(sorted(bottles)) or "none"
        raise BottleError(f"no bottle named {name!r} (bottles: {known})")
    return bottles[name]


def _save(bottle: Bottle) -> None:
    bottles = load()
    bottles[bottle.name] = bottle
    _write(bottles)


def _forget(name: str) -> None:
    bottles = load()
    bottles.pop(name, None)
    _write(bottles)


def _write(bottles: dict[str, Bottle]) -> None:
    write_json(registry_path(), {"version": 1, "bottles": {b.name: asdict(b) for b in bottles.values()}})


def default_name(repo: str, taken: set[str]) -> str:
    """The repo's name, then repo-2, repo-3, ... for further bottles."""
    if repo not in taken:
        return repo
    n = 2
    while f"{repo}-{n}" in taken:
        n += 1
    return f"{repo}-{n}"


def create(repo_name: str, image: str, branch: str | None = None, name: str | None = None) -> Bottle:
    repo = repos.get(repo_name)
    start = repos.Start(branch, repos.resolve_branch(repo, branch)) if branch else repos.default_start(repo)
    existing = load()
    name = name or default_name(repo.name, set(existing))
    if not repos.NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid name {name!r}: use letters, digits, '.', '_' or '-'")
    if name in existing:
        raise BottleError(f"a bottle named {name!r} already exists")
    prereqs.ensure_container()
    images.ensure_built(image)

    bottle = Bottle(name, uuid.uuid4().hex, repo.name, image, start.branch, start.commit, time.time())
    _save(bottle)
    try:
        git("update-ref", bottle.ref, bottle.commit, "", repo=repo.path)
        runtime.network_create(bottle.network)
        proxy = daemon.proxy_url(runtime.network_gateway(bottle.network), daemon.EGRESS_PORT)
        runtime.container_run(
            bottle.container, images.tag(image), bottle.network,
            env=_proxy_env(proxy),
            mounts=[runtime.Mount(repos.objects_dir(repo), OBJECTS_MOUNT)],
        )
        # The gateway only exists once the container is on the network, so egress comes second.
        daemon.ensure_egress(bottle.name, bottle.network)
        _init_workspace(bottle)
    except BaseException as failure:
        try:
            delete(bottle.name, force=True)
        except Exception as cleanup:
            raise BottleError(f"{_describe(failure)}; cleanup also failed: {cleanup}") from failure
        raise
    bottle = replace(bottle, status="ready")
    _save(bottle)
    return bottle


def _proxy_env(proxy: str) -> dict[str, str]:
    env = {}
    for key in ("http_proxy", "https_proxy"):
        env[key] = env[key.upper()] = proxy
    env["no_proxy"] = env["NO_PROXY"] = NO_PROXY
    return env


def _init_workspace(bottle: Bottle) -> None:
    """Check out the bottle's branch (or detached commit) at /workspace, borrowing the mounted objects."""
    image, commit = shlex.quote(bottle.image), shlex.quote(bottle.commit)
    if bottle.branch:
        branch = shlex.quote(bottle.branch)
        init, point_head = f"git init -q -b {branch} {WORKSPACE}", f"update-ref refs/heads/{branch} {commit}"
    else:
        init, point_head = f"git init -q {WORKSPACE}", f"update-ref --no-deref HEAD {commit}"
    script = f"""
        set -e
        command -v git >/dev/null || {{ echo "image "{image}" has no git" >&2; exit 1; }}
        {init}
        echo {OBJECTS_MOUNT} > {WORKSPACE}/.git/objects/info/alternates
        git -C {WORKSPACE} {point_head}
        git -C {WORKSPACE} reset -q --hard
    """
    try:
        runtime.container_exec(bottle.container, ["sh", "-c", script], user=USER)
    except BottleError as e:
        raise BottleError(f"setting up {WORKSPACE} failed: {e}") from None


def start(name: str) -> Bottle:
    return ensure_running(name)


def stop(name: str) -> None:
    """Stop the bottle's VM and egress. Its checkout and changes are kept."""
    bottle = get(name)
    daemon.release_egress(bottle.name)
    if runtime.container_state(bottle.container) == "running":
        runtime.container_stop(bottle.container)


def shutdown() -> tuple[list[str], bool]:
    """Stop every running bottle, then bottled. Returns the stopped bottles and whether bottled was running."""
    stopped = []
    for bottle in load().values():
        if runtime.container_state(bottle.container) == "running":
            runtime.container_stop(bottle.container)
            stopped.append(bottle.name)
    return stopped, daemon.stop()


def ensure_running(name: str) -> Bottle:
    """Start the bottle's container and egress if they aren't running."""
    bottle = get(name)
    if bottle.status != "ready":
        raise BottleError(f"{name} is {bottle.status}; remove it with `bottle delete {name}`")
    state = runtime.container_state(bottle.container)
    if state is None:
        raise BottleError(f"{name}'s container is gone; remove it with `bottle delete {name}`")
    if state != "running":
        prereqs.ensure_container()
        runtime.container_start(bottle.container)
    daemon.ensure_egress(bottle.name, bottle.network)
    return bottle


def shell(name: str):
    bottle = ensure_running(name)
    runtime.container_exec_interactive(bottle.container, ["bash", "-l"], user=USER, workdir=WORKSPACE)


def fetched_prefix(bottle: Bottle) -> str:
    """Where fetched branches land in the wrapped repo; `git branch -r` shows them as bottle-NAME/..."""
    return f"refs/remotes/bottle-{bottle.name}"


@dataclass(frozen=True)
class Update:
    """One ref `bottle git fetch` wrote, or refused to write, in the wrapped repo."""

    ref: str  # e.g. refs/remotes/bottle-gradle/main
    kind: str  # "new", "updated", "forced" or "rejected"
    old: str | None
    new: str


@dataclass(frozen=True)
class FetchResult:
    updates: list[Update]
    commit: str | None = None  # set when a bare commit was fetched into FETCH_HEAD
    # Branches fetched before that the bottle no longer has; kept in the wrapped repo.
    gone: list[str] = field(default_factory=list)


# git fetch --porcelain flags: https://git-scm.com/docs/git-fetch#_output
_PORCELAIN_KINDS = {"*": "new", " ": "updated", "+": "forced", "!": "rejected"}


def fetch(name: str, rev: str | None = None, force: bool = False) -> FetchResult:
    """Fetch the bottle's work into the wrapped repo, without ever losing anything there.

    Only adds or fast-forwards bottle-NAME/* refs: history the bottle rewrote
    is refused (unless `force`, which needs a single branch), and branches
    deleted in the bottle stay (reported as gone). The host's own branches are
    never touched.

    No rev: every branch, plus a detached HEAD as bottle-NAME/detached/<commit>.
    A branch: just that branch. Any other revision: that commit, into FETCH_HEAD.
    """
    bottle = ensure_running(name)
    repo = repos.get(bottle.repo)
    prefix = fetched_prefix(bottle)
    if rev is None:
        if force:
            raise BottleError("--force needs a single branch: bottle git fetch NAME BRANCH --force")
        refspecs = [f"refs/heads/*:{prefix}/*"]
        head = _detached_head(bottle)
        if head:
            refspecs.append(f"{head}:{prefix}/detached/{head[:12]}")
        updates = _git_fetch(bottle, repo, refspecs)
        gone = sorted(set(_fetched_branches(bottle, repo)) - {b for b, _ in _bottle_branches(bottle)})
        return _checked(FetchResult(updates, gone=gone), name)

    if _in_bottle_succeeds(bottle, ["git", "-C", WORKSPACE, "show-ref", "--verify", "--quiet", f"refs/heads/{rev}"]):
        refspec = f"{'+' if force else ''}refs/heads/{rev}:{prefix}/{rev}"
        return _checked(FetchResult(_git_fetch(bottle, repo, [refspec])), name)

    if force:
        raise BottleError(f"--force needs a branch; {rev!r} isn't a branch in {name}")
    try:
        commit = _in_bottle(bottle, ["git", "-C", WORKSPACE, "rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}"])
    except BottleError:
        raise BottleError(f"{name} has no branch or commit {rev!r}") from None
    # Only refs are advertised; fetching a bare commit needs upload-pack to allow it.
    _git_fetch(bottle, repo, [commit], upload_pack=["git", "-c", "uploadpack.allowAnySHA1InWant=true", "upload-pack"])
    return FetchResult([], commit)


def unfetched_work(bottle: Bottle) -> list[str]:
    """What deleting the bottle would lose: branches (or a detached HEAD) whose
    commits the wrapped repo doesn't have, and uncommitted changes."""
    repo = repos.get(bottle.repo)
    tips = _bottle_branches(bottle)
    head = _detached_head(bottle)
    if head:
        tips.append(("detached HEAD", head))
    lost = [f"branch {b}" if b != "detached HEAD" else b for b, commit in tips if not _host_has(repo, commit)]
    if _in_bottle(bottle, ["git", "-C", WORKSPACE, "status", "--porcelain"]):
        lost.append("uncommitted changes")
    return lost


def _bottle_branches(bottle: Bottle) -> list[tuple[str, str]]:
    out = _in_bottle(bottle, ["git", "-C", WORKSPACE, "for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads"])
    return [tuple(line.split(" ")) for line in out.splitlines()]


def _fetched_branches(bottle: Bottle, repo: repos.Repo) -> list[str]:
    prefix = fetched_prefix(bottle)
    out = git("for-each-ref", "--format=%(refname)", f"{prefix}/", repo=repo.path)
    names = [ref.removeprefix(f"{prefix}/") for ref in out.splitlines()]
    return [n for n in names if not n.startswith("detached/")]


def _host_has(repo: repos.Repo, commit: str) -> bool:
    try:
        git("cat-file", "-e", f"{commit}^{{commit}}", repo=repo.path)
        return True
    except BottleError:
        return False


def _detached_head(bottle: Bottle) -> str | None:
    if _in_bottle_succeeds(bottle, ["git", "-C", WORKSPACE, "symbolic-ref", "--quiet", "HEAD"]):
        return None  # on a branch, which refs/heads/* already covers
    return _in_bottle(bottle, ["git", "-C", WORKSPACE, "rev-parse", "HEAD"])


def _checked(result: FetchResult, name: str) -> FetchResult:
    rejected = [u for u in result.updates if u.kind == "rejected"]
    if rejected:
        lines = "\n".join(f"  {u.ref.removeprefix('refs/remotes/')}: {u.old[:12]} -> {u.new[:12]}" for u in rejected)
        branch = rejected[0].ref.split("/", 3)[3]
        raise BottleError(
            f"{name} rewrote history; refusing to overwrite what was fetched before:\n{lines}\n"
            f"Other refs were fetched. To overwrite one: bottle git fetch {name} {branch} --force"
        )
    return result


def _git_fetch(bottle: Bottle, repo: repos.Repo, refspecs: list[str], upload_pack: list[str] | None = None) -> list[Update]:
    # git's ext:: transport runs upload-pack in the bottle over `container exec`:
    # no network or keys involved. %S is the service git asks for (git-upload-pack).
    remote = "ext::" + " ".join(runtime.exec_command(bottle.container, [*(upload_pack or ["%S"]), WORKSPACE], user=USER))
    cmd = ["git", "-C", str(repo.path), "-c", "protocol.ext.allow=always", "fetch", "--porcelain", "--no-tags", remote, *refspecs]
    result = subprocess.run(cmd, capture_output=True, text=True)
    updates = []
    for line in result.stdout.splitlines():
        flag, (old, new, ref) = line[0], line[2:].split(" ")
        updates.append(Update(ref, _PORCELAIN_KINDS.get(flag, flag), None if set(old) == {"0"} else old, new))
    # A rejected ref makes fetch exit non-zero; that's reported via the updates.
    if result.returncode != 0 and not any(u.kind == "rejected" for u in updates):
        raise BottleError(result.stderr.strip() or "git fetch failed")
    return updates


def _in_bottle(bottle: Bottle, argv: list[str]) -> str:
    return runtime.container_exec(bottle.container, argv, user=USER).strip()


def _in_bottle_succeeds(bottle: Bottle, argv: list[str]) -> bool:
    try:
        runtime.container_exec(bottle.container, argv, user=USER)
        return True
    except BottleError:
        return False


def list_all() -> list[tuple[Bottle, str]]:
    """Every bottle with its current state: the container's, unless setup never finished."""
    result = []
    for bottle in sorted(load().values(), key=lambda b: b.created):
        state = bottle.status if bottle.status != "ready" else (runtime.container_state(bottle.container) or "missing")
        result.append((bottle, state))
    return result


def delete(name: str, force: bool = False) -> None:
    """Remove every part of the bottle that exists, then its record.

    Refuses, unless `force`, if the bottle has work the wrapped repo doesn't:
    unfetched commits or uncommitted changes. A stopped bottle is started to
    check. Each step tolerates its part being absent, so this also finishes off
    a bottle left half-made or half-deleted. If a step fails, the record stays
    (marked broken) so the next delete can retry.
    """
    bottle = get(name)
    if not force and bottle.status == "ready" and runtime.container_state(bottle.container) is not None:
        try:
            ensure_running(name)
            lost = unfetched_work(bottle)
        except BottleError as e:
            raise BottleError(f"couldn't check {name} for unfetched work ({e}); delete anyway with --force") from None
        if lost:
            hints = []
            if "uncommitted changes" in lost:
                hints.append("commit the changes in the bottle")
            if any(item != "uncommitted changes" for item in lost) or hints:
                hints.append(f"fetch with `bottle git fetch {name}`")
            raise BottleError(
                f"{name} has work its repo doesn't: {', '.join(lost)}. "
                f"To keep it, {' and '.join(hints)}; or delete anyway with --force"
            )
    failures = []
    for step, action in (
        ("egress", lambda: daemon.release_egress(bottle.name)),
        ("container", lambda: runtime.container_delete(bottle.container)),
        ("network", lambda: runtime.network_delete(bottle.network)),
        ("ref", lambda: _delete_ref(bottle)),
    ):
        try:
            action()
        except Exception as e:  # keep going: clean up every part we can
            failures.append(f"{step}: {e}")
    if failures:
        _save(replace(bottle, status="broken"))
        raise BottleError(f"couldn't fully delete {name} ({'; '.join(failures)}); rerun `bottle delete {name}`")
    _forget(name)


def _delete_ref(bottle: Bottle) -> None:
    try:
        repo = repos.get(bottle.repo)
    except BottleError:
        return  # repo was unwrapped; nothing left to clean in it
    if repo.path.is_dir():
        git("update-ref", "-d", bottle.ref, repo=repo.path)


def _describe(failure: BaseException) -> str:
    return "interrupted" if isinstance(failure, KeyboardInterrupt) else str(failure)
