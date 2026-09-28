"""Bottles: a repo checked out in an image, running in its own VM.

The registry at $BOTTLE_HOME/bottles.json is written before anything is
created, so a failure at any point leaves a record of what may exist.
Creation rolls back on failure; delete() removes whatever parts exist, so it
also cleans up a bottle that was left half-made.

Each bottle has:
  - a host-only network, bottle-<id>, reaching only the host
  - an egress proxy on that network's gateway, served by bottled, which also
    serves the repo as the bottle's origin and host remotes (see host.py)
  - a container, bottle-<name>, with the repo's objects mounted read-only
  - a checkout at /workspace that borrows those objects via alternates

Objects the bottle uses from the repo aren't protected from `git gc` on the
host: if the host deletes a branch a bottle checked out and gc later prunes
its commits (after git's grace periods, weeks to months), that checkout breaks
and `bottle reset` starts the bottle over.
"""

import shlex
import subprocess
import sys
import time
import uuid
from contextlib import contextmanager
from pathlib import Path
from dataclasses import asdict, dataclass, field, replace
from typing import NoReturn

from bottle import auth, daemon, features as features_, host, images, prereqs, repos, runtime
from bottle.errors import BottleError
from bottle.git import git
from bottle.store import bottle_home, namespace, read_json, write_json

OBJECTS_MOUNT = "/mnt/repo/objects"
WORKSPACE = "/workspace"
USER = "genie"
NO_PROXY = "localhost,127.0.0.1"
CONTEXT_FILE = "BOTTLE.md"  # in genie's home
GIT_URL = f"http://{host.HOST}{host.GIT_PATH}"

# What bottle relies on inside every bottle, checked (as genie) whenever one
# starts. Documented in containers/images/base/Dockerfile; keep them in sync.
CONTRACT = (
    ("tini is PID 1", '[ "$(cat /proc/1/comm)" = tini ]'),
    ("bottle-entrypoint is installed", "test -x /usr/local/bin/bottle-entrypoint"),
    ("the genie user has UID 1000", '[ "$(id -un)" = genie ] && [ "$(id -u)" = 1000 ]'),
    ("genie has passwordless sudo", "sudo -n true"),
    ("/workspace exists and genie can write to it", "test -d /workspace && test -w /workspace"),
    ("git is installed", "command -v git"),
    ("sshd is installed", "test -x /usr/sbin/sshd"),
)


@dataclass(frozen=True)
class Request:
    """What `bottle new` was asked for, so `bottle reset` can ask again."""

    branch: str | None = None  # None: the repo's default branch, resolved again on reset
    features: tuple[str, ...] = ()  # beyond the repo's defaults, as canonical specs

    def __post_init__(self) -> None:
        object.__setattr__(self, "features", tuple(self.features))


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
    # Features installed on top of the image, as specs (e.g. jvm:version=17), including dependencies.
    features: tuple[str, ...] = ()
    request: Request | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "features", tuple(self.features))  # a list, when read from JSON
        if isinstance(self.request, dict):
            object.__setattr__(self, "request", Request(**self.request))
        elif self.request is None:  # recorded before bottle kept requests
            object.__setattr__(self, "request", Request(self.branch))

    @property
    def environment(self) -> str:
        """The image and its features, for display: base+claude+jvm."""
        return "+".join([self.image, *sorted(self.features)])

    @property
    def checkout(self) -> str:
        """What's checked out, for display: the branch, or the detached commit."""
        return self.branch or f"({self.commit[:12]})"

    @property
    def network(self) -> str:
        return f"bottle-{self.id}"

    @property
    def container(self) -> str:
        return f"bottle-{namespace()}{self.name}"

    @property
    def owner(self) -> tuple[str, str]:
        """What proves a container is this bottle's: its label value and network."""
        return self.id, self.network


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


def create(
    repo_name: str, image: str = images.BASE, branch: str | None = None, name: str | None = None, features: list[str] = ()
) -> Bottle:
    request = Request(branch, repos.canonical_features(list(features)))
    repo, start, installed = _plan(repo_name, request)
    existing = load()
    name = name or default_name(repo.name, set(existing))
    if not repos.NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid name {name!r}: use letters, digits, '.', '_' or '-'")
    if name in existing:
        raise BottleError(f"a bottle named {name!r} already exists")
    prereqs.ensure_container()
    tag = features_.ensure_built(image, installed)
    auth.ensure_logged_in(installed)

    bottle = Bottle(
        name, uuid.uuid4().hex, repo.name, image, start.branch, start.commit, time.time(),
        features=tuple(installed), request=request,
    )
    # Never build on a name something else is using: rollback would then be cleaning up after it.
    if runtime.container_info(bottle.container) is not None:
        raise BottleError(f"a container named {bottle.container} already exists; choose another name with --name")
    _save(bottle)
    try:
        runtime.network_create(bottle.network, {runtime.OWNER_LABEL: bottle.id})
        _run_container(bottle, repo, tag)
    except BaseException as failure:
        try:
            delete(bottle.name, force=True, keep_image=True)
        except Exception as cleanup:
            raise BottleError(f"{_describe(failure)}; cleanup also failed: {cleanup}") from failure
        raise
    bottle = replace(bottle, status="ready")
    _save(bottle)
    return bottle


def _plan(repo_name: str, request: Request) -> tuple[repos.Repo, repos.Start, list[str]]:
    """What `request` means right now: the repo, where to start, and every feature to install."""
    repo = repos.get(repo_name)
    branch = request.branch
    start = repos.Start(branch, repos.resolve_branch(repo, branch)) if branch else repos.default_start(repo)
    installed = [f.spec for f in features_.resolve(merge_features(repo.features, list(request.features)))]
    return repo, start, installed


def features_on_reset(bottle: Bottle) -> list[str]:
    """The features `bottle reset` would give the bottle now: its repo's defaults plus its own."""
    return _plan(bottle.repo, bottle.request)[2]


def _run_container(bottle: Bottle, repo: repos.Repo, tag: str) -> None:
    """Start a fresh container for the bottle from `tag`, and set it up from scratch."""
    proxy = daemon.proxy_url(runtime.network_gateway(bottle.network), daemon.EGRESS_PORT)
    runtime.container_run(
        bottle.container, tag, bottle.network,
        env=_proxy_env(proxy),
        mounts=[runtime.Mount(repos.objects_dir(repo), OBJECTS_MOUNT)],
        labels={runtime.OWNER_LABEL: bottle.id},
    )
    verify_contract(bottle)
    # The gateway only exists once the container is on the network, so egress comes second.
    daemon.ensure_egress(bottle.name, bottle.network, git_dir(bottle))
    _init_workspace(bottle)
    deliver_credentials(bottle)


def deliver_credentials(bottle: Bottle) -> None:
    """Put the credentials the bottle's features declare into /etc/environment, for SSH sessions.

    Runs whenever the bottle starts, so logging in or out reaches it on its
    next start. Values go in on stdin, never on a command line; a credential
    that isn't set (anymore) is removed.
    """
    env = auth.env_for(bottle.features)
    if not env:
        return
    lines = "".join(f"{key}={value or ''}\n" for key, value in env.items())
    script = (
        'while IFS= read -r line; do name=${line%%=*}; sed -i "/^$name=/d" /etc/environment; '
        '[ "$line" = "$name=" ] || printf "%s\\n" "$line" >> /etc/environment; done'
    )
    runtime.container_exec(bottle.container, ["sh", "-c", script], user="root", input=lines)


@contextmanager
def throwaway(feature_specs: list[str]):
    """A short-lived bottle with just these features and network access, and no repo.

    Yields a function turning a command into the command line that runs it in
    the throwaway bottle, attached to a terminal. Everything is removed on exit.
    """
    tag = features_.ensure_built(images.BASE, feature_specs)
    token = uuid.uuid4().hex[:12]
    name = f"bottle-throwaway-{token}"
    try:
        runtime.network_create(name, {runtime.OWNER_LABEL: token})
        proxy = daemon.proxy_url(runtime.network_gateway(name), daemon.EGRESS_PORT)
        runtime.container_run(name, tag, name, env=_proxy_env(proxy), mounts=[], labels={runtime.OWNER_LABEL: token})
        daemon.ensure_egress(name, name)
        yield lambda argv: runtime.exec_command(name, argv, user=USER, tty=True)
    finally:
        for cleanup in (
            lambda: daemon.release_egress(name),
            lambda: runtime.container_delete(name, (token, name)),
            lambda: runtime.network_delete(name),
        ):
            try:
                cleanup()
            except Exception as e:
                print(f"bottle: cleaning up {name}: {e}", file=sys.stderr)


def reset(name: str, force: bool = False) -> Bottle:
    """`bottle delete NAME` then `bottle new` with the arguments it was created with, in one step.

    So the bottle gets its repo's current default features (plus any it was
    created with), the latest commit of its branch, and a fresh, running VM.
    Refuses, unless `force`, if the bottle has work its repo doesn't. Unlike a
    real delete and new, it keeps the bottle's id and network, and changes
    nothing until the new image is built. Also repairs a bottle left half-made.
    """
    bottle = get(name)
    repo, start, installed = _plan(bottle.repo, bottle.request)
    auth.ensure_logged_in(installed)
    if not force and bottle.status == "ready" and runtime.container_state(bottle.container) is not None:
        _refuse_to_lose_work(bottle, "reset")
    prereqs.ensure_container()
    tag = features_.ensure_built(bottle.image, installed)
    fresh = replace(
        bottle, branch=start.branch, commit=start.commit, created=time.time(), status="creating",
        features=tuple(installed),
    )
    daemon.release_egress(bottle.name)
    runtime.container_delete(bottle.container, bottle.owner)
    _save(fresh)
    try:
        if not runtime.network_exists(fresh.network):
            runtime.network_create(fresh.network, {runtime.OWNER_LABEL: fresh.id})
        _run_container(fresh, repo, tag)
    except BottleError as e:
        raise BottleError(f"resetting {name} failed: {e}; rerun `bottle reset {name}`, or delete it") from None
    fresh = replace(fresh, status="ready")
    _save(fresh)
    if set(installed) != set(bottle.features):
        _delete_image(bottle)  # the old features' image, unless another bottle uses it
    return fresh


def merge_features(defaults: tuple[str, ...] | list[str], extra: list[str]) -> list[str]:
    """The repo's default features plus `extra`; an extra spec for a default feature replaces it."""
    merged = {features_.parse_spec(spec)[0]: spec for spec in defaults}
    for spec in extra:
        merged[features_.parse_spec(spec)[0]] = spec
    return list(merged.values())


def git_dir(bottle: Bottle) -> Path | None:
    """The git dir of the bottle's repo, served to it as origin; None if the repo is gone."""
    try:
        return repos.objects_dir(repos.get(bottle.repo)).parent
    except BottleError:
        return None


def _proxy_env(proxy: str) -> dict[str, str]:
    env = {}
    for key in ("http_proxy", "https_proxy"):
        env[key] = env[key.upper()] = proxy
    env["no_proxy"] = env["NO_PROXY"] = NO_PROXY
    return env


def context(bottle: Bottle) -> str:
    """What the agent should know about its bottle, written to ~/BOTTLE.md when its workspace is set up.

    Agent-neutral: agent features point their own instruction files at it
    (the claude feature links ~/.claude/CLAUDE.md to it). Nothing in it changes
    over the bottle's life (no commits), so it never goes stale.
    """
    at = f"on the `{bottle.branch}` branch" if bottle.branch else "at a detached commit"
    ns = ref_namespace(bottle)
    return (
        "# Bottle\n\n"
        "You're running in a bottle: a sandboxed Linux VM.\n\n"
        f"- You're `{USER}`, with passwordless sudo.\n"
        f"- `{WORKSPACE}` is a checkout of the `{bottle.repo}` repo, {at}. It has two remotes:\n"
        "  - `origin`: the repo's upstream, up to date whenever you fetch. Read-only.\n"
        "  - `host`: the repo's local branches on the host. Hand your work back with `git push host`, "
        f"which works as it would anywhere: your pushes land in `{ns}`, a git namespace of this "
        "bottle's own, so you can't reach the host's branches and don't have to avoid them. "
        "Deleting a ref is refused; force pushes are allowed, to be used as deliberately as ever.\n"
        "- The network is reachable only through the HTTP proxy in `HTTPS_PROXY`/`HTTP_PROXY` (already set). "
        "There's no DNS, and private addresses are blocked.\n"
    )


def _init_workspace(bottle: Bottle) -> None:
    """Check out the bottle's branch (or detached commit) at /workspace, borrowing the mounted objects."""
    commit = shlex.quote(bottle.commit)
    if bottle.branch:
        branch = shlex.quote(bottle.branch)
        init, point_head = f"git init -q -b {branch} {WORKSPACE}", f"update-ref refs/heads/{branch} {commit}"
    else:
        init, point_head = f"git init -q {WORKSPACE}", f"update-ref --no-deref HEAD {commit}"
    script = f"""
        set -e
        {init}
        echo {OBJECTS_MOUNT} > {WORKSPACE}/.git/objects/info/alternates
        git -C {WORKSPACE} {point_head}
        git -C {WORKSPACE} reset -q --hard
        # Two remotes, served live through the egress proxy (see host.py): origin is the
        # repo's upstream (read-only), host is the repo's local branches. Nothing to
        # configure for pushing: the host puts receive-pack in this bottle's git
        # namespace, so ordinary pushes land there and can't name anything else.
        git -C {WORKSPACE} remote add origin {GIT_URL}/origin
        git -C {WORKSPACE} remote add host {GIT_URL}/host
        git -C {WORKSPACE} config checkout.defaultRemote origin
        git -C {WORKSPACE} fetch -q --multiple origin host
        printf '%s' {shlex.quote(context(bottle))} > "$HOME/{CONTEXT_FILE}"
    """
    try:
        runtime.container_exec(bottle.container, ["sh", "-c", script], user=USER)
    except BottleError as e:
        raise BottleError(f"setting up {WORKSPACE} failed: {e}") from None


def start(name: str) -> Bottle:
    auth.ensure_logged_in(get(name).features)
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
        raise BottleError(f"{name}'s container is gone; recreate it with `bottle reset {name}`, or `bottle delete {name}`")
    if state != "running":
        prereqs.ensure_container()
        runtime.container_start(bottle.container)
        verify_contract(bottle)
        deliver_credentials(bottle)
    daemon.ensure_egress(bottle.name, bottle.network, git_dir(bottle))
    return bottle


def verify_contract(bottle: Bottle) -> None:
    """Fail loudly unless the running bottle provides everything bottle relies on (CONTRACT)."""
    checks = "\n".join(f"sh -c {shlex.quote(test)} >/dev/null 2>&1 || echo {shlex.quote(what)}" for what, test in CONTRACT)
    try:
        missing = _in_bottle(bottle, ["sh", "-c", f"# bottle contract\n{checks}"])
    except BottleError as e:
        missing = f"couldn't run the checks as {USER} ({e})"
    if missing:
        problems = "; ".join(missing.splitlines())
        raise BottleError(f"{bottle.environment} doesn't meet the bottle contract: {problems}")


def shell(name: str) -> NoReturn:
    """Open a login shell in the bottle, at /workspace."""
    _attach(name, ["bash", "-l"], tty=True)


def exec_(name: str, argv: list[str]) -> NoReturn:
    """Run one command in the bottle, at /workspace, and exit with its status.

    A TTY only when this process has one on both ends, so output stays clean
    when it's piped or redirected.
    """
    if not argv:
        raise BottleError("bottle exec needs a command: bottle exec NAME COMMAND [ARG...]")
    _attach(name, argv, tty=sys.stdin.isatty() and sys.stdout.isatty())


def _attach(name: str, argv: list[str], tty: bool) -> NoReturn:
    """Replace this process with `argv` running in the bottle, starting it if stopped."""
    auth.ensure_logged_in(get(name).features)
    bottle = ensure_running(name)
    env = {key: value for key, value in auth.env_for(bottle.features).items() if value}
    runtime.container_exec_interactive(bottle.container, argv, user=USER, workdir=WORKSPACE, env=env, tty=tty)


def ref_namespace(bottle: Bottle) -> str:
    """The git namespace a bottle's pushes are written into, on the host (see host.py).

    receive-pack runs with GIT_NAMESPACE set to this, so the bottle reads and
    writes refs/namespaces/<it>/ and nothing else; hooks/post-receive mirrors
    what arrives to branches under the same name, for now.
    """
    return f"bottle-{bottle.name}"


def unsaved_work(bottle: Bottle) -> list[str]:
    """What deleting the bottle would lose: branches (or a detached HEAD) whose
    commits the bottle's repo doesn't have, and uncommitted changes."""
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


def _refuse_to_lose_work(bottle: Bottle, action: str) -> None:
    """Raise unless the bottle's work is all in its repo. Starts a stopped bottle to check."""
    name = bottle.name
    try:
        ensure_running(name)
        lost = unsaved_work(bottle)
    except BottleError as e:
        raise BottleError(f"couldn't check {name} for unsaved work ({e}); {action} anyway with --force") from None
    if lost:
        hints = []
        if "uncommitted changes" in lost:
            hints.append("commit the changes in the bottle")
        if any(item != "uncommitted changes" for item in lost) or hints:
            hints.append(f"push it from the bottle (`git push host`), or `bottle exec {name} git push host`")
        raise BottleError(
            f"{name} has work its repo doesn't: {', '.join(lost)}. "
            f"To keep it, {' and '.join(hints)}; or {action} anyway with --force"
        )


def delete(name: str, force: bool = False, keep_image: bool = False) -> None:
    """Remove every part of the bottle that exists, then its record, and its image if no other bottle uses it.

    Refuses, unless `force`, if the bottle has work the bottle's repo doesn't:
    unpushed commits or uncommitted changes. A stopped bottle is started to
    check. Each step tolerates its part being absent, so this also finishes off
    a bottle left half-made or half-deleted. If a step fails, the record stays
    (marked broken) so the next delete can retry.
    """
    bottle = get(name)
    if not force and bottle.status == "ready" and runtime.container_state(bottle.container) is not None:
        _refuse_to_lose_work(bottle, "delete")
    failures = []
    for step, action in (
        ("egress", lambda: daemon.release_egress(bottle.name)),
        ("container", lambda: runtime.container_delete(bottle.container, bottle.owner)),
        ("network", lambda: runtime.network_delete(bottle.network)),
    ):
        try:
            action()
        except Exception as e:  # keep going: clean up every part we can
            failures.append(f"{step}: {e}")
    if failures:
        _save(replace(bottle, status="broken"))
        raise BottleError(f"couldn't fully delete {name} ({'; '.join(failures)}); rerun `bottle delete {name}`")
    _forget(name)
    if not keep_image:
        _delete_image(bottle)


def _delete_image(bottle: Bottle) -> None:
    """Delete the bottle's features image if no container uses it, and any stale images.

    Best effort: the bottle is already gone. The base image is kept, since every
    bottle is built on it.
    """
    try:
        if bottle.features:
            tag = features_.image_tag(bottle.image, features_.resolve(list(bottle.features)))
            in_use = runtime.images_in_use()
            if any(ref.name == tag and ref.digest not in in_use for ref in runtime.images()):
                runtime.image_delete(tag)
        features_.remove_stale()
    except BottleError as e:
        print(f"bottle: couldn't remove {bottle.name}'s image: {e}", file=sys.stderr)


def _describe(failure: BaseException) -> str:
    return "interrupted" if isinstance(failure, KeyboardInterrupt) else str(failure)
