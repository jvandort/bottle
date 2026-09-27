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
import time
import uuid
from dataclasses import asdict, dataclass, replace

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
            delete(bottle.name)
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


def list_all() -> list[tuple[Bottle, str]]:
    """Every bottle with its current state: the container's, unless setup never finished."""
    result = []
    for bottle in sorted(load().values(), key=lambda b: b.created):
        state = bottle.status if bottle.status != "ready" else (runtime.container_state(bottle.container) or "missing")
        result.append((bottle, state))
    return result


def delete(name: str) -> None:
    """Remove every part of the bottle that exists, then its record.

    Each step tolerates its part being absent, so this also finishes off a
    bottle left half-made or half-deleted. If a step fails, the record stays
    (marked broken) so the next delete can retry.
    """
    bottle = get(name)
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
