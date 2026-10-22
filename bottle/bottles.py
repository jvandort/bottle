"""Bottles: a sandbox, in its own VM, made from a spec and served the git remotes it's given.

The registry at $BOTTLE_HOME/bottles.json is written before anything is
created, so a failure at any point leaves a record of what may exist.
Creation rolls back on failure; delete() removes whatever parts exist, so it
also cleans up a bottle that was left half-made.

Each bottle has:
  - a host-only network, bottle-<id>, reaching only the host, and held up by
    the anchor (see anchor.py) for as long as it exists
  - an egress proxy on that network's gateway, served by bottled, which also
    serves the bottle's git remotes (see host.py)
  - a container, bottle-<name>, with its repo's objects mounted read-only, if
    it has a repo
  - a checkout at /workspace that borrows those objects via alternates

What it installs, and its memory, come from a spec its creator owns (see
specs.py). Its remotes are the ones it was created with, and nothing is
registered in the repo: what the bottle pushes is for its creator to read.
What's worth keeping before a bottle is deleted is its creator's call too.

Objects the bottle uses from the repo aren't protected from `git gc` on the
host: if the host deletes a branch a bottle checked out and gc later prunes
its commits (after git's grace periods, weeks to months), that checkout
breaks, and the bottle has to be made again.
"""

import logging
import re
import shlex
import subprocess
import sys
import time
import uuid
from contextlib import contextmanager
from pathlib import Path
from dataclasses import asdict, dataclass, field, replace
from typing import NoReturn

from bottle import anchor, auth, ca, daemon, features as features_, host, images, prereqs, repos, runtime, specs
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
NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
# The remotes a bottle with a repo and no --git-remote is served, read-only.
ORIGIN, HOST_REMOTE = "origin", "host"

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
class Bottle:
    name: str
    id: str
    image: str
    branch: str | None  # None: a detached commit
    commit: str  # "" when there's no checkout: a bottle with no repo
    created: float
    # "creating" until setup finishes, "ready" after; "broken" if cleanup failed.
    status: str = "creating"
    # Features installed on top of the image, as specs (e.g. jvm:version=17), including dependencies.
    features: tuple[str, ...] = ()
    # The VM's memory, as a `container run --memory` value (e.g. 8G); None: all of the machine's.
    memory: str | None = None
    # The spec it was made from, canonical.
    spec: dict = field(default_factory=dict)
    # Its repo, the toplevel path its objects are mounted from; None: no repo.
    git_repo: str | None = None
    # The remotes it's served.
    remotes: tuple[host.Remote, ...] = ()
    # Its creator's addition to ~/BOTTLE.md (`bottle new --context`).
    context: str = ""

    def __post_init__(self) -> None:
        object.__setattr__(self, "features", tuple(self.features))  # a list, when read from JSON
        object.__setattr__(self, "remotes", tuple(  # dicts, when read from JSON
            host.Remote(**r) if isinstance(r, dict) else r for r in self.remotes))

    @property
    def environment(self) -> str:
        """The image and its features, for display: base+claude+jvm."""
        return "+".join([self.image, *sorted(self.features)])

    @property
    def checkout(self) -> str:
        """What's checked out, for display: the branch, or the detached commit."""
        return self.branch or (f"({self.commit[:12]})" if self.commit else "-")

    @property
    def network(self) -> str:
        return f"bottle-{self.id}"

    @property
    def container(self) -> str:
        return f"bottle-{namespace()}{self.name}"


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


def create(
    spec: dict, name: str, git_repo: Path | None = None, remotes: list[str] | None = None,
    checkout: str | None = None, context: str = "", image: str = images.BASE,
) -> Bottle:
    """Create a bottle from a spec: its features and memory, from a file its creator owns (see specs.py).

    `name` is the creator's choice, and the bottle's identity: there's no
    default, so a creator never has to find out what it got, and a taken name
    is refused rather than numbered. `git_repo` is the repo whose objects are
    mounted and whose remotes are served; without one the bottle has an empty
    /workspace and no remotes.
    `remotes` are `--git-remote` specs (see host.Remote); None serves origin
    and host, read-only. `checkout` is a branch or commit of `git_repo`
    (default: origin's default branch, else what the repo has checked out).
    Nothing is registered in the repo: the bottle's creator reads what it
    pushes, from the namespaces it chose.
    """
    spec = specs.canonical(spec)
    installed = [f.spec for f in features_.resolve(list(spec["features"]))]
    if git_repo is None:
        if remotes or checkout:
            raise BottleError("--git-remote and --checkout need --git-repo")
        repo, start, served = None, repos.Start(None, ""), ()
    else:
        path = repos.toplevel(git_repo)
        repo = repos.Repo(repos.derive_name(path), path)
        start = _start_at(repo, checkout)
        served = _parse_remotes(remotes)
    existing = load()
    if not NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid name {name!r}: use letters, digits, '.', '_' or '-'")
    if name in existing:
        raise BottleError(f"a bottle named {name!r} already exists")
    if repo is not None:
        _refuse_a_second_writer(repo, served, existing.values())
    prereqs.ensure_container()
    tag = features_.ensure_built(image, installed)
    auth.ensure_logged_in(installed)

    bottle = Bottle(
        name, uuid.uuid4().hex, image, start.branch, start.commit, time.time(),
        features=tuple(installed), memory=specs.memory_limit(spec), spec=spec,
        git_repo=str(repo.path) if repo else None, remotes=served, context=context,
    )
    bottle = _bring_up(bottle, repo, tag)
    log.info("%s: created, %s, features %s, network %s", name,
             f"{repo.path} at {bottle.checkout}" if repo else "no repo", ", ".join(installed) or "none", bottle.network)
    return bottle


def _bring_up(bottle: Bottle, repo: repos.Repo | None, tag: str) -> Bottle:
    """Create the bottle's network and container from `tag`, and set it up; roll it all back on failure."""
    # Never build on a name something else is using: rollback would then be cleaning up after it.
    if runtime.container_info(bottle.container) is not None:
        raise BottleError(f"a container named {bottle.container} already exists; choose another name with --name")
    _save(bottle)
    try:
        runtime.network_create(bottle.network, {runtime.OWNER_LABEL: bottle.id})
        _anchor()
        _run_container(bottle, repo, tag)
    except BaseException as failure:
        log.warning("%s: creating failed, rolling back: %s", bottle.name, _describe(failure))
        try:
            delete(bottle.name)
        except Exception as cleanup:
            raise BottleError(f"{_describe(failure)}; cleanup also failed: {cleanup}") from failure
        raise
    bottle = replace(bottle, status="ready")
    _save(bottle)
    return bottle


def _start_at(repo: repos.Repo, checkout: str | None) -> repos.Start:
    """Where `--checkout` starts: a branch (the repo's, else origin's), else any commit; by default origin's
    default branch, else what the repo has checked out."""
    if checkout is None:
        return repos.default_start(repo)
    try:
        return repos.Start(checkout, repos.resolve_branch(repo, checkout))
    except BottleError:
        pass
    try:
        return repos.Start(None, git("rev-parse", "--verify", "--quiet", f"{checkout}^{{commit}}", repo=repo.path))
    except BottleError:
        raise BottleError(f"{repo.path} has no branch or commit {checkout!r}") from None


def _parse_remotes(specs_: list[str] | None) -> tuple[host.Remote, ...]:
    """The remotes a bottle is served: those given, or origin and host, read-only."""
    if specs_ is None:
        return (host.Remote(ORIGIN, host.UPSTREAM), host.Remote(HOST_REMOTE, host.HEADS))
    remotes = tuple(host.Remote.parse(spec) for spec in specs_)
    names = [r.name for r in remotes]
    for name in sorted({n for n in names if names.count(n) > 1}):
        raise BottleError(f"remote {name} given twice")
    pushed = [r.push_namespace for r in remotes if r.push]
    for namespace in sorted({n for n in pushed if pushed.count(n) > 1}):
        raise BottleError(f"two remotes push into namespace {namespace}; each namespace has one writer")
    return remotes


def _refuse_a_second_writer(repo: repos.Repo, remotes: tuple[host.Remote, ...], others) -> None:
    """Refuse a namespace another bottle already pushes into, in the same repo: each has one writer."""
    ours = {r.push_namespace for r in remotes if r.push}
    if not ours:
        return
    mine = repos.objects_dir(repo)
    for other in others:
        theirs = {r.push_namespace for r in other.remotes if r.push}
        shared = ours & theirs
        if shared and _objects(other) == mine:
            raise BottleError(
                f"bottle {other.name} already pushes into namespace {min(shared)} of {repo.path}; "
                "each namespace has one writer"
            )


def _objects(bottle: Bottle) -> Path | None:
    directory = git_dir(bottle)
    return directory / "objects" if directory else None


def _anchor(excluding: str | None = None) -> None:
    """Have the anchor on every bottle's network but `excluding`'s, which is about to be deleted."""
    anchor.ensure({b.network for b in load().values() if b.name != excluding})


def _run_container(bottle: Bottle, repo: repos.Repo | None, tag: str) -> None:
    """Start a fresh container for the bottle from `tag`, and set it up from scratch."""
    proxy = daemon.proxy_url(runtime.network_gateway(bottle.network), daemon.EGRESS_PORT)
    # The bottle gets stand-ins, never credentials: the real ones are held by
    # the egress proxy (see auth.py), and a throwaway bottle gets neither.
    runtime.container_run(
        bottle.container, tag, bottle.network,
        env=_container_env(proxy, bottle.features),
        mounts=[runtime.Mount(repos.objects_dir(repo), OBJECTS_MOUNT)] if repo else [],
        labels={runtime.OWNER_LABEL: bottle.id}, memory=bottle.memory,
    )
    verify_contract(bottle)
    trust_egress_ca(bottle)
    # The gateway only exists once the container is on the network, so egress comes second.
    daemon.ensure_egress(bottle.name, bottle.network, git_dir(bottle), bottle.features, bottle.remotes)
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
            lambda: runtime.container_delete(name, token),
            lambda: runtime.network_delete(name),
        ):
            try:
                cleanup()
            except Exception as e:
                log.warning("%s: cleaning up: %s", name, e)
                print(f"bottle: cleaning up {name}: {e}", file=sys.stderr)
        log.info("%s: throwaway bottle removed", name)


def git_dir(bottle: Bottle) -> Path | None:
    """The git dir of the bottle's repo, which its remotes are served from; None if it has none, or it's gone."""
    try:
        return repos.objects_dir(repos.Repo("", Path(bottle.git_repo))).parent if bottle.git_repo else None
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


def context(bottle: Bottle) -> str:
    """What the agent should know about its bottle, written to ~/BOTTLE.md when its workspace is set up:
    what the bottle is, what its features give it, and nothing about how to work in it.

    Agent-neutral: agent features point their own instruction files at it
    (the claude feature links ~/.claude/CLAUDE.md to it).

    How to work -- branches, pushing, merging -- is its creator's to say, in
    the context it adds; this says what's there.
    """
    lines = [f"# Bottle\n\nYou're `{USER}`, running in a bottle: a sandboxed Linux VM with passwordless sudo.\n"]
    if bottle.git_repo:
        at = f"on the `{bottle.branch}` branch" if bottle.branch else "at a detached commit"
        lines.append(f"- `{WORKSPACE}` is a checkout of `{Path(bottle.git_repo).name}`, {at}.")
        if bottle.remotes:
            lines.append("  Its remotes are served by the host:")
            lines += [f"  - `{r.name}`: {_describe_remote(r)}" for r in bottle.remotes]
        else:
            lines.append("  It has no remotes.")
    else:
        lines.append(f"- `{WORKSPACE}` is empty: this bottle has no repo.")
    lines.append("- The network is reachable only through the HTTP proxy in `HTTPS_PROXY`/`HTTP_PROXY`\n"
                 "  (already set). There's no DNS, and private addresses are blocked.")
    # Each feature's own word on what it gives the agent (customizations.bottle.context).
    told = [(f.id, f.agent_context()) for f in features_.resolve(list(bottle.features))]
    told = [(feature_id, text) for feature_id, text in told if text]
    if told:
        lines.append("\n## Features")
        lines += [f"\n### Feature '{feature_id}'\n\n{text}" for feature_id, text in told]
    text = "\n".join(lines) + "\n"
    return text + (f"\n{bottle.context}" if bottle.context else "")


def _describe_remote(remote: host.Remote) -> str:
    if remote.fetch == host.UPSTREAM:
        what = "the repo's upstream, up to date whenever you fetch."
    elif remote.fetch == host.HEADS:
        what = "the user's own branches, as they stand in their repo."
    else:
        what = "refs the host keeps for this bottle."
    if remote.push is None:
        return f"{what} Read-only."
    # Fetching reads other refs than pushing writes (a two-lane exchange, say), so say so,
    # or an agent fetching to check its push finds nothing and pushes again.
    echo = "" if remote.push == remote.fetch else " Fetching it doesn't show your own pushes."
    return f"{what} You can push to it: pushes can't rewrite or delete what's already there.{echo}"


def _init_workspace(bottle: Bottle) -> None:
    """Check out the bottle's branch or commit at /workspace, borrowing the mounted objects, with its remotes.

    Without a repo, just the context. No branch tracks anything: which remote
    a branch pulls from is its creator's to decide.
    """
    script = ["set -e"]
    if bottle.git_repo:
        commit = shlex.quote(bottle.commit)
        if bottle.branch:
            branch = shlex.quote(bottle.branch)
            script += [f"git init -q -b {branch} {WORKSPACE}"]
            point_head = f"update-ref refs/heads/{branch} {commit}"
        else:
            script += [f"git init -q {WORKSPACE}"]
            point_head = f"update-ref --no-deref HEAD {commit}"
        script += [
            f"echo {OBJECTS_MOUNT} > {WORKSPACE}/.git/objects/info/alternates",
            f"git -C {WORKSPACE} {point_head}",
            f"git -C {WORKSPACE} reset -q --hard",
        ]
        names = [r.name for r in bottle.remotes]
        script += [f"git -C {WORKSPACE} remote add {name} {GIT_URL}/{name}" for name in names]
        if ORIGIN in names:
            script += [f"git -C {WORKSPACE} config checkout.defaultRemote {ORIGIN}"]
        if names:
            script += [f"git -C {WORKSPACE} fetch -q --multiple {' '.join(names)}"]
    script += [f"printf '%s' {shlex.quote(context(bottle))} > \"$HOME/{CONTEXT_FILE}\""]
    try:
        runtime.container_exec(bottle.container, ["sh", "-c", "\n".join(script)], user=USER)
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
    """Stop every running bottle, the anchor, then bottled. Returns the stopped bottles and whether bottled was running."""
    stopped = []
    if runtime.services_running():
        for bottle in load().values():
            if runtime.container_state(bottle.container) == "running":
                runtime.container_stop(bottle.container)
                log.info("%s: stopped (shutdown)", bottle.name)
                stopped.append(bottle.name)
        anchor.remove()
    return stopped, daemon.stop()


def ensure_running(name: str) -> Bottle:
    """Start the bottle's container and egress if they aren't running."""
    bottle = get(name)
    if bottle.status != "ready":
        raise BottleError(f"{name} is {bottle.status}; remove it with `bottle delete {name}`")
    state = runtime.container_state(bottle.container)
    if state is None:
        raise BottleError(f"{name}'s container is gone; `bottle delete {name}`, then `bottle new` again")
    if state == "running" and not runtime.host_has_address(runtime.network_gateway(bottle.network)):
        # Running but cut off: another network took its bridge (see anchor.py). Restarting
        # the VM would take the bridge back and cut that network off instead; only
        # restarting container's services gives each network a bridge of its own again.
        log.warning("%s: running, but this machine has no address on %s (its bridge is gone)", name, bottle.network)
        raise BottleError(
            f"{name}'s network lost its bridge on this machine, an apple/container bug; to repair it, "
            "run `bottle shutdown && container system stop`, then try again"
        )
    _anchor()
    if state != "running":
        prereqs.ensure_container()
        runtime.container_start(bottle.container)
        log.info("%s: started", name)
        verify_contract(bottle)
        trust_egress_ca(bottle)
        point_at_egress(bottle)
    daemon.ensure_egress(bottle.name, bottle.network, git_dir(bottle), bottle.features, bottle.remotes)
    return bottle


def current_proxy_env(bottle: Bottle) -> dict[str, str]:
    """The proxy settings for the bottle's network as it is now.

    The ones `container run` gave it can be stale: a network's subnet isn't
    kept when container's services restart (as when the machine does), so the
    gateway the egress proxy listens on can move.
    """
    return _proxy_env(daemon.proxy_url(runtime.network_gateway(bottle.network), daemon.EGRESS_PORT))


def point_at_egress(bottle: Bottle) -> None:
    """Point SSH sessions, which only see /etc/environment, at the egress proxy's current address."""
    env = current_proxy_env(bottle)
    script = f'for var in {" ".join(env)}; do sed -i "/^$var=/d" /etc/environment; done; cat >> /etc/environment'
    try:
        runtime.container_exec(
            bottle.container, ["sh", "-c", script], user="root", input="".join(f"{k}={v}\n" for k, v in env.items())
        )
    except BottleError as e:
        raise BottleError(f"pointing {bottle.container} at its egress proxy failed: {e}") from None


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
    runtime.container_exec_interactive(
        bottle.container, argv, user=USER, workdir=WORKSPACE, tty=tty, env=current_proxy_env(bottle)
    )


def _in_bottle(bottle: Bottle, argv: list[str]) -> str:
    return runtime.container_exec(bottle.container, argv, user=USER).strip()


def list_all() -> list[tuple[Bottle, str]]:
    """Every bottle with its current state: the container's, unless setup never finished."""
    result = []
    for bottle in sorted(load().values(), key=lambda b: b.created):
        state = bottle.status if bottle.status != "ready" else (runtime.container_state(bottle.container) or "missing")
        result.append((bottle, state))
    return result


def status(names: list[str] = ()) -> list[dict]:
    """Every bottle's state and what it was made from, or just those `names`, for `bottle status --json`.

    A name with no bottle gets a row with state "missing", so a caller can
    tell a bottle that's gone from a command that failed.
    """
    states = {bottle.name: (bottle, state) for bottle, state in list_all()}
    rows = []
    for name in names or list(states):
        if name not in states:
            rows.append({"name": name, "state": "missing"})
            continue
        bottle, state = states[name]
        rows.append({
            "name": bottle.name, "state": state, "created": bottle.created,
            "git_repo": bottle.git_repo,
            "branch": bottle.branch, "commit": bottle.commit or None,
            "features": sorted(bottle.features), "memory": bottle.memory or specs.ALL_MEMORY, "spec": bottle.spec,
            "remotes": [r.spec for r in bottle.remotes],
        })
    return rows


def delete(name: str) -> None:
    """Remove every part of the bottle that exists, then its record, and any images gone stale.

    Each step tolerates its part being absent, so this also finishes off
    a bottle left half-made or half-deleted. If a step fails, the record stays
    (marked broken) so the next delete can retry.
    """
    bottle = get(name)
    failures = []
    for step, action in (
        ("egress", lambda: daemon.release_egress(bottle.name)),
        ("container", lambda: runtime.container_delete(bottle.container, bottle.id)),
        ("anchor", lambda: _anchor(excluding=bottle.name)),  # off the network, or it can't be deleted
        ("network", lambda: runtime.network_delete(bottle.network)),
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
    _remove_stale_images()


def _remove_stale_images() -> None:
    """Delete images whose features or base have changed since they were built, if no container uses one.

    An image that's still current is kept, even with nothing using it: it's a
    cache, and the next bottle with those features starts without a build.
    Best effort: the bottle is already gone.
    """
    try:
        features_.remove_stale()
    except BottleError as e:
        print(f"bottle: couldn't remove stale images: {e}", file=sys.stderr)


def _describe(failure: BaseException) -> str:
    return "interrupted" if isinstance(failure, KeyboardInterrupt) else str(failure)
