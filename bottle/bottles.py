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

import logging
import shlex
import subprocess
import sys
import time
import uuid
from contextlib import contextmanager
from pathlib import Path
from dataclasses import asdict, dataclass, field, replace
from typing import NoReturn

from bottle import auth, ca, daemon, features as features_, host, images, prereqs, repos, runtime
from bottle.errors import BottleError
from bottle.git import git
from bottle.store import bottle_home, namespace, read_json, write_json

log = logging.getLogger("bottle.bottles")

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
    ("update-ca-certificates is installed", "test -x /usr/sbin/update-ca-certificates"),
)
# Where the egress CA's certificate goes in a bottle: the base image points
# NODE_EXTRA_CA_CERTS here too, for tools that don't read the system store.
CA_CERTIFICATE = "/usr/local/share/ca-certificates/bottle-egress.crt"


@dataclass(frozen=True)
class Request:
    """What `bottle new` was asked for, so `bottle reset` can ask again."""

    branch: str | None = None  # None: the repo's default branch, resolved again on reset
    features: tuple[str, ...] = ()  # beyond the repo's defaults, as canonical specs
    memory: str | None = None  # None: the repo's; else canonical, which may be "all"

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
    # The VM's memory, as a `container run --memory` value (e.g. 8G); None: all of the machine's.
    memory: str | None = None
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
    repo_name: str, image: str = images.BASE, branch: str | None = None, name: str | None = None,
    features: list[str] = (), memory: str | None = None,
) -> Bottle:
    request = Request(branch, repos.canonical_features(list(features)), memory and repos.canonical_memory(memory))
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
        features=tuple(installed), memory=_memory(repo, request), request=request,
    )
    # Never build on a name something else is using: rollback would then be cleaning up after it.
    if runtime.container_info(bottle.container) is not None:
        raise BottleError(f"a container named {bottle.container} already exists; choose another name with --name")
    _save(bottle)
    try:
        runtime.network_create(bottle.network, {runtime.OWNER_LABEL: bottle.id})
        _run_container(bottle, repo, tag)
        register_remote(repo, bottle)  # so the repo can fetch what the bottle pushes
    except BaseException as failure:
        log.warning("%s: creating failed, rolling back: %s", bottle.name, _describe(failure))
        try:
            delete(bottle.name, force=True, keep_image=True)
        except Exception as cleanup:
            raise BottleError(f"{_describe(failure)}; cleanup also failed: {cleanup}") from failure
        raise
    bottle = replace(bottle, status="ready")
    _save(bottle)
    log.info("%s: created from %s at %s (%s), features %s, network %s",
             name, repo.name, start.branch, start.commit[:12], ", ".join(installed) or "none", bottle.network)
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


def memory_on_reset(bottle: Bottle) -> str | None:
    """The memory `bottle reset` would give the bottle now: its own, else its repo's."""
    return _memory(repos.get(bottle.repo), bottle.request)


def _memory(repo: repos.Repo, request: Request) -> str | None:
    if request.memory is None:
        return repo.memory
    return None if request.memory == repos.ALL_MEMORY else request.memory


def _run_container(bottle: Bottle, repo: repos.Repo, tag: str) -> None:
    """Start a fresh container for the bottle from `tag`, and set it up from scratch."""
    proxy = daemon.proxy_url(runtime.network_gateway(bottle.network), daemon.EGRESS_PORT)
    # The bottle gets stand-ins, never credentials: the real ones are held by
    # the egress proxy (see auth.py), and a throwaway bottle gets neither.
    runtime.container_run(
        bottle.container, tag, bottle.network,
        env=_container_env(proxy, bottle.features),
        mounts=[runtime.Mount(repos.objects_dir(repo), OBJECTS_MOUNT)],
        labels={runtime.OWNER_LABEL: bottle.id}, memory=bottle.memory,
    )
    verify_contract(bottle)
    trust_egress_ca(bottle)
    # The gateway only exists once the container is on the network, so egress comes second.
    daemon.ensure_egress(bottle.name, bottle.network, git_dir(bottle), bottle.features)
    _init_workspace(bottle)


@contextmanager
def throwaway(feature_specs: list[str]):
    """A short-lived bottle with just these features and network access, and no repo.

    Yields a function turning a command into the command line that runs it in
    the throwaway bottle, attached to a terminal. Everything is removed on exit.
    """
    tag = features_.ensure_built(images.BASE, feature_specs)
    token = uuid.uuid4().hex[:12]
    name = f"bottle-throwaway-{token}"
    log.info("%s: throwaway bottle with %s", name, ", ".join(feature_specs) or "no features")
    try:
        runtime.network_create(name, {runtime.OWNER_LABEL: token})
        proxy = daemon.proxy_url(runtime.network_gateway(name), daemon.EGRESS_PORT)
        runtime.container_run(name, tag, name, env=_proxy_env(proxy), mounts=[], labels={runtime.OWNER_LABEL: token})
        # No features, so no credentials and no egress CA: a login starts from
        # nothing, and mustn't find its own credential attached to what it sends.
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
                log.warning("%s: cleaning up: %s", name, e)
                print(f"bottle: cleaning up {name}: {e}", file=sys.stderr)
        log.info("%s: throwaway bottle removed", name)


def reset(name: str, force: bool = False) -> Bottle:
    """`bottle delete NAME` then `bottle new` with the arguments it was created with, in one step.

    So the bottle gets its repo's current default features (plus any it was
    created with) and memory (unless it was created with its own), the latest commit of its branch, and a fresh, running VM.
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
        features=tuple(installed), memory=_memory(repo, bottle.request),
    )
    daemon.release_egress(bottle.name)
    runtime.container_delete(bottle.container, bottle.owner)
    _save(fresh)
    try:
        if not runtime.network_exists(fresh.network):
            runtime.network_create(fresh.network, {runtime.OWNER_LABEL: fresh.id})
        _run_container(fresh, repo, tag)
        register_remote(repo, fresh)  # also repairs a bottle made before its repo had lanes
    except BottleError as e:
        log.warning("%s: resetting failed: %s", name, e)
        raise BottleError(f"resetting {name} failed: {e}; rerun `bottle reset {name}`, or delete it") from None
    fresh = replace(fresh, status="ready")
    _save(fresh)
    log.info("%s: reset to %s at %s (%s), features %s",
             name, repo.name, start.branch, start.commit[:12], ", ".join(installed) or "none")
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


def _container_env(proxy: str, features: tuple[str, ...] | list[str]) -> dict[str, str]:
    """What `container run` sets for a bottle: the proxy, and the stand-ins its features asked for.

    `container exec` sees these; SSH sessions only see /etc/environment, so the
    base image's entrypoint mirrors them there, and BOTTLE_MIRROR_ENV tells it
    which ones beyond the proxy variables to mirror.
    """
    standins = auth.standins_for(features)
    if not standins:
        return _proxy_env(proxy)
    return _proxy_env(proxy) | standins | {"BOTTLE_MIRROR_ENV": " ".join(sorted(standins))}


CONTEXT = """\
# Bottle

You're `{user}`, running in a bottle: a sandboxed Linux VM with passwordless sudo.

- `{workspace}` is a checkout of the `{repo}` repo, {at}. It has three remotes:
  - `origin`: the repo's upstream, up to date whenever you fetch. Read-only.
  - `host`: the user's own branches, as they stand in their repo. Read-only;
    fetch it to see what they've integrated.
  - `work`: how you and the user exchange commits. `git push work` writes your
    lane and `git pull work` reads theirs -- one writer each, so neither side's
    push is ever refused for what the other did. You don't read your own pushes
    back from it; what you fetch is always the user's.
- The network is reachable only through the HTTP proxy in `HTTPS_PROXY`/`HTTP_PROXY`
  (already set). There's no DNS, and private addresses are blocked.

## Starting work

A bottle is often started on the branch the work belongs on, and committing
there is expected. However, when beginning a new line of work, or when what's
checked out is a branch unrelated to the new work, create a new branch first.
Name it yourself -- you don't need to ask -- and once you've pushed it, tell
the user what it's called and how to pick it up:

    git fetch {remote} && git switch <your branch>

## Working with the user

You and the user both work that branch, from separate repos and sharing no
disk. The two lanes are a conversation, and the history they leave is the
record of it. That record is not intended to be a tidy log, but a transcript
of the collaboration between you and the user.

- **Pull when you're given a prompt**, and again before you push, in case
  something arrived while you were working.
- **Push as you go, not when you're finished.** Every push is also the only
  backup this work has.
- **The merges are yours.** The user may commit from a base older than your
  last push, deliberately: it hands you the merge. Resolve it, say what you
  decided, and carry on.
- **Messy history is expected here.** Merge commits, typos in messages, commits
  that undo earlier ones -- that's the shape of a real exchange, not something
  to apologise for or offer to tidy up.

## What can't be rewritten

- **Your lane is append-only**: force-pushes and deletes are refused. To undo
  anything you've pushed -- a bad commit, a stray file, something the user asks
  you to remove -- commit the undo on top. Here, removing something means adding
  a commit that removes it, never retracting what's already there.
- **The user's commits are theirs.** Your work goes on top of them. Once you've
  pushed, merge their commits rather than rebasing onto them; rebase only what
  you haven't handed over yet.
- **A new branch is always open to you**, because its name is new. Start one
  whenever you begin a separate line of work -- it costs nothing, and it's also
  the way out of a history you'd rather stop building on. Final delivered work
  also belongs on a fresh branch.

## Delivering finished work

Develop on your working branch, messy history and all. When the user explicitly
asks for it -- never on your own initiative -- squash the work into a clean
history and push it to a **new** branch with `git push work`: the name they
give you, or `<your branch>-clean` if they didn't name one. Your working branch
stays exactly as it is -- the clean branch is built beside it, never in place.
"""


def context(bottle: Bottle) -> str:
    """What the agent should know about its bottle, written to ~/BOTTLE.md when its workspace is set up.

    Often the only thing an agent is ever told about the protocol, so it says
    what the lanes are for and not just what they refuse: the messy history is
    the point, the merges are the agent's, and undoing means committing forward.

    Agent-neutral: agent features point their own instruction files at it
    (the claude feature links ~/.claude/CLAUDE.md to it). Nothing in it changes
    over the bottle's life (no commits), so it never goes stale.
    """
    at = f"on the `{bottle.branch}` branch" if bottle.branch else "at a detached commit"
    return CONTEXT.format(user=USER, workspace=WORKSPACE, repo=bottle.repo, at=at, remote=host_remote(bottle))


def _init_workspace(bottle: Bottle) -> None:
    """Check out the bottle's branch (or detached commit) at /workspace, borrowing the mounted objects."""
    commit = shlex.quote(bottle.commit)
    upstream = ":"
    if bottle.branch:
        branch = shlex.quote(bottle.branch)
        init, point_head = f"git init -q -b {branch} {WORKSPACE}", f"update-ref refs/heads/{branch} {commit}"
        # So bare `git push` and `git pull` reach the right lane. Set by hand
        # rather than by --set-upstream: work/<branch> doesn't exist until
        # somebody pushes, and this has to work before anybody has.
        upstream = (f"git -C {WORKSPACE} config branch.{branch}.remote work\n"
                    f"        git -C {WORKSPACE} config branch.{branch}.merge refs/heads/{branch}")
    else:
        init, point_head = f"git init -q {WORKSPACE}", f"update-ref --no-deref HEAD {commit}"
    script = f"""
        set -e
        {init}
        echo {OBJECTS_MOUNT} > {WORKSPACE}/.git/objects/info/alternates
        git -C {WORKSPACE} {point_head}
        git -C {WORKSPACE} reset -q --hard
        # Three remotes, served live through the egress proxy (see host.py): origin is
        # the repo's upstream and host the user's branches, both read-only; work is
        # where the two sides exchange commits. Nothing to configure for pushing --
        # the host serves each direction from a different git namespace, so ordinary
        # pushes land in this bottle's lane and can't name anything else.
        git -C {WORKSPACE} remote add origin {GIT_URL}/origin
        git -C {WORKSPACE} remote add host {GIT_URL}/host
        git -C {WORKSPACE} remote add work {GIT_URL}/work
        git -C {WORKSPACE} config checkout.defaultRemote origin
        {upstream}
        git -C {WORKSPACE} fetch -q --multiple origin host work
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
    if not runtime.services_running():
        return  # so nothing is running
    if runtime.container_state(bottle.container) == "running":
        runtime.container_stop(bottle.container)
        log.info("%s: stopped", name)


def shutdown() -> tuple[list[str], bool]:
    """Stop every running bottle, then bottled. Returns the stopped bottles and whether bottled was running."""
    stopped = []
    for bottle in load().values() if runtime.services_running() else ():
        if runtime.container_state(bottle.container) == "running":
            runtime.container_stop(bottle.container)
            log.info("%s: stopped (shutdown)", bottle.name)
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
    if state == "running" and not runtime.host_has_address(runtime.network_gateway(bottle.network)):
        # Running but cut off: its network's bridge is gone. Restarting the VM brings it back.
        log.warning("%s: running, but this machine has no address on %s (its bridge is gone); restarting", name, bottle.network)
        print(f"bottle: {name}'s network lost its bridge on this machine; restarting {name}...", file=sys.stderr)
        daemon.release_egress(bottle.name)
        runtime.container_stop(bottle.container)
        state = "stopped"
    if state != "running":
        prereqs.ensure_container()
        runtime.container_start(bottle.container)
        log.info("%s: started", name)
        verify_contract(bottle)
        trust_egress_ca(bottle)
    daemon.ensure_egress(bottle.name, bottle.network, git_dir(bottle), bottle.features)
    return bottle


def trust_egress_ca(bottle: Bottle) -> None:
    """Make the bottle's trust store hold its egress CA's certificate, and no other.

    Every time a bottle starts, so a bottle made before its CA, or whose
    credential hosts have changed since, trusts the current one, and a bottle
    with no credential hosts trusts none. It's what lets the egress proxy
    attach a credential to the bottle's HTTPS (see ca.py); the key stays on
    the machine.
    """
    certificate = ca.certificate(bottle.name, auth.intercepted_hosts(bottle.features))
    if certificate is None:
        script = f"if [ -e {CA_CERTIFICATE} ]; then rm {CA_CERTIFICATE} && update-ca-certificates --fresh >/dev/null; fi"
    else:
        script = (
            f"cat > {CA_CERTIFICATE}.new && "
            f"if cmp -s {CA_CERTIFICATE}.new {CA_CERTIFICATE}; then rm {CA_CERTIFICATE}.new; "
            f"else mv {CA_CERTIFICATE}.new {CA_CERTIFICATE} && update-ca-certificates --fresh >/dev/null; fi"
        )
    try:
        runtime.container_exec(bottle.container, ["sh", "-c", script], user="root", input=certificate)
    except BottleError as e:
        raise BottleError(f"installing the egress CA in {bottle.container} failed: {e}") from None


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
    runtime.container_exec_interactive(bottle.container, argv, user=USER, workdir=WORKSPACE, tty=tty)


def host_remote(bottle: Bottle) -> str:
    """What the host repo calls this bottle, as a git remote."""
    return f"bottle-{bottle.name}"


def register_remote(repo: repos.Repo, bottle: Bottle) -> None:
    """Make the bottle a remote of its repo: fetch its lane, push to the host's.

    One remote, two lanes. `uploadpack` and `receivepack` name the command git
    runs on the remote side, so pointing them at different namespaces means
    `git fetch` reads what the bottle wrote and `git push` writes what it reads,
    with nothing else to configure: `git switch BRANCH` creates a tracking
    branch by itself, because exactly one remote has that name.

    Safe to call on a bottle that already has one, so it can repair a remote
    that's missing or edited: every setting is written with --replace-all,
    which collapses however many values a key has to the one value here.
    """
    name = host_remote(bottle)
    out, in_ = host.lane(bottle.name, host.OUT), host.lane(bottle.name, host.IN)
    settings = {
        "url": str(repo.path),
        "fetch": f"+refs/heads/*:refs/remotes/{name}/*",
        "uploadpack": f"git --namespace={out} upload-pack",
        # The host's own lane, so no restrictions: this is your repo, and
        # rewriting what you haven't handed over yet is yours to do.
        "receivepack": f"git -c receive.denyCurrentBranch=ignore --namespace={in_} receive-pack",
    }
    for key, value in settings.items():
        git("config", "--replace-all", f"remote.{name}.{key}", value, repo=repo.path)


def forget_remote(bottle: Bottle) -> None:
    """Drop the remote; the lanes' refs stay, since they may be the only copy.

    Resolves the repo itself so that it, too, is allowed to be missing: this
    runs as a step of delete, which has to be able to finish whatever state it
    finds, and a bottle that couldn't clean up its remote would be one nothing
    could remove.
    """
    try:
        git("remote", "remove", host_remote(bottle), repo=repos.get(bottle.repo).path)
    except BottleError:
        pass  # the repo is gone, or the remote was never registered, or already dropped


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
            hints.append(f"push it from the bottle (`git push work`), or `bottle exec {name} git push work`")
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
        ("remote", lambda: forget_remote(bottle)),
        ("egress CA", lambda: ca.forget(bottle.name)),
    ):
        try:
            action()
        except Exception as e:  # keep going: clean up every part we can
            failures.append(f"{step}: {e}")
    if failures:
        log.warning("%s: deleting left parts behind: %s", name, "; ".join(failures))
        _save(replace(bottle, status="broken"))
        raise BottleError(f"couldn't fully delete {name} ({'; '.join(failures)}); rerun `bottle delete {name}`")
    _forget(name)
    log.info("%s: deleted", name)
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
