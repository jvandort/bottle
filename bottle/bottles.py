"""Bottles: a sandbox, in its own VM, made from a spec and served the git remotes it's given.

The registry at $BOTTLE_HOME/bottles.json is written before anything is
created, so a failure at any point leaves a record of what may exist.
Creation rolls back on failure; delete() removes whatever parts exist, so it
also cleans up a bottle that was left half-made.

Each bottle has:
  - a smolvm machine, bottle-<name> (see smolvm.py), with each of its mounts'
    repos' objects mounted read-only
  - a token at bottled's decider, which smolvm asks about each of the
    machine's connections before making them, and which serves each mount's git
    remotes at bottle.host (see decider.py, egress.py, host.py)
  - for each mount, a checkout at its path that borrows those objects via
    alternates. Which paths is the bottle's creator's to say; none is special,
    but the first is where a shell or a command starts (genie's home without one).

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
import os
import re
import shlex
import sys
import time
import uuid
from contextlib import contextmanager
from pathlib import Path
from dataclasses import asdict, dataclass, field, fields, replace
from typing import NoReturn

from bottle import auth, ca, daemon, features as features_, host, images, prereqs, repos, smolvm, specs
from bottle.errors import BottleError
from bottle.git import git
from bottle.store import bottle_home, namespace, read_json, write_json

log = logging.getLogger("bottle.bottles")

OBJECTS_ROOT = "/mnt/objects"   # a mount at /x has its objects at /mnt/objects/x
USER = "genie"
HOME = f"/home/{USER}"
# A mount's path: absolute, of name-like segments, so it's safe in a URL and a shell.
MOUNT_PATH = re.compile(r"(?:/[A-Za-z0-9][A-Za-z0-9._-]*)+")
# How a bottle made before mounts had its one repo: at /workspace, its objects at
# /mnt/repo/objects, its remotes at http://bottle.host/git/<remote>.
LEGACY_PATH, LEGACY_OBJECTS = "/workspace", "/mnt/repo/objects"
CONTEXT_FILE = "BOTTLE.md"  # in genie's home
GIT_URL = f"http://{host.HOST}{host.GIT_PATH}"
NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
# The remotes a bottle with a repo and no --git-remote is served, read-only.
ORIGIN, HOST_REMOTE = "origin", "host"

# What bottle relies on inside every bottle, checked (as genie) whenever one
# starts. Documented in containers/images/base/Dockerfile; keep them in sync.
CONTRACT = (
    ("bottle-entrypoint is installed", "test -x /usr/local/bin/bottle-entrypoint"),
    ("the genie user has UID 1000", '[ "$(id -un)" = genie ] && [ "$(id -u)" = 1000 ]'),
    ("genie has passwordless sudo", "sudo -n true"),
    ("git is installed", "command -v git"),
    ("sshd is installed", "test -x /usr/sbin/sshd"),
    ("update-ca-certificates is installed", "test -x /usr/sbin/update-ca-certificates"),
)
# Where the egress CA's certificate goes in a bottle: the base image points
# NODE_EXTRA_CA_CERTS here too, for tools that don't read the system store.
CA_CERTIFICATE = "/usr/local/share/ca-certificates/bottle-egress.crt"


@dataclass(frozen=True)
class Mount:
    """A repo in the bottle: its objects mounted read-only, checked out at `path`
    borrowing them, and served the remotes it was given."""

    path: str  # where it's checked out, absolute
    repo: str  # its toplevel on the host, whose objects are mounted
    branch: str | None  # None: a detached commit
    commit: str
    remotes: tuple[host.Remote, ...] = ()
    objects: str = ""  # where its objects are mounted in the bottle (default: under OBJECTS_ROOT)
    at: str | None = None  # where its remotes are under bottle.host/git (default: its path, less the /)

    def __post_init__(self) -> None:
        known = {f.name for f in fields(host.Remote)}
        object.__setattr__(self, "remotes", tuple(  # dicts, when read from JSON; options since removed are dropped
            host.Remote(**{k: v for k, v in r.items() if k in known}) if isinstance(r, dict) else r
            for r in self.remotes))
        if not self.objects:
            object.__setattr__(self, "objects", OBJECTS_ROOT + self.path)
        if self.at is None:
            object.__setattr__(self, "at", self.path.lstrip("/"))

    @property
    def checkout(self) -> str:
        """What's checked out, for display: the branch, or the detached commit."""
        return self.branch or f"({self.commit[:12]})"

    def url(self, remote: str) -> str:
        return f"{GIT_URL}/{self.at}/{remote}" if self.at else f"{GIT_URL}/{remote}"


@dataclass(frozen=True)
class MountRequest:
    """A mount as `bottle new` is asked for one: the repo, where, from what, served what."""

    path: str
    repo: Path
    checkout: str | None = None  # a branch or commit (default: origin's default branch, else what's checked out)
    remotes: list[str] | None = None  # --git-remote specs, less the path (default: origin and host, read-only)


@dataclass(frozen=True)
class Bottle:
    name: str
    id: str
    image: str
    created: float
    # "creating" until setup finishes, "ready" after; "broken" if cleanup failed.
    status: str = "creating"
    # Features installed on top of the image, as specs (e.g. jvm:version=17), including dependencies.
    features: tuple[str, ...] = ()
    # The VM's memory, a size in M or G (e.g. 8G), from its spec. None only in records from before specs had to say.
    memory: str | None = None
    # The spec it was made from, canonical.
    spec: dict = field(default_factory=dict)
    # Its repos, each checked out at its path; the first is where a shell starts.
    mounts: tuple[Mount, ...] = ()
    # Its creator's addition to ~/BOTTLE.md (`bottle new --context`).
    context: str = ""
    # The image archive its machine was made from, which is kept while it exists.
    archive: str | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "features", tuple(self.features))  # a list, when read from JSON
        object.__setattr__(self, "mounts", tuple(  # dicts, when read from JSON
            Mount(**m) if isinstance(m, dict) else m for m in self.mounts))

    @property
    def environment(self) -> str:
        """The image and its features, for display: base+claude+jvm."""
        return "+".join([self.image, *sorted(self.features)])

    @property
    def workdir(self) -> str:
        """Where a shell or a command starts: the first mount, else genie's home."""
        return self.mounts[0].path if self.mounts else HOME

    @property
    def machine(self) -> str:
        return f"bottle-{namespace()}{self.name}"


def registry_path():
    return bottle_home() / "bottles.json"


def load() -> dict[str, Bottle]:
    data = read_json(registry_path()) or {"bottles": {}}
    known = {f.name for f in fields(Bottle)}
    # Keys a record from an older bottle may have (its driver, say) are left out.
    return {name: Bottle(**{k: v for k, v in _mounted(entry).items() if k in known})
            for name, entry in data["bottles"].items()}


def _mounted(entry: dict) -> dict:
    """A record from before mounts, with its one repo as the mount it always was."""
    if "mounts" in entry or not entry.get("git_repo"):
        return entry
    return {**entry, "mounts": [{
        "path": LEGACY_PATH, "repo": entry["git_repo"], "branch": entry.get("branch"),
        "commit": entry.get("commit", ""), "remotes": entry.get("remotes", []),
        "objects": LEGACY_OBJECTS, "at": "",
    }]}


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
    spec: dict, name: str, mounts: list[MountRequest] | None = None, context: str = "", image: str = images.BASE,
) -> Bottle:
    """Create a bottle from a spec: its features and memory, from a file its creator owns (see specs.py).

    `name` is the creator's choice, and the bottle's identity: there's no
    default, so a creator never has to find out what it got, and a taken name
    is refused rather than numbered. Each mount is a repo whose objects are
    mounted and whose remotes are served, checked out at its path; without
    any, the bottle has no repo. Nothing is registered in a repo: the bottle's
    creator reads what it pushes, from the namespaces it chose.
    """
    spec = specs.canonical(spec)
    installed = [f.spec for f in features_.resolve(list(spec["features"]))]
    existing = load()
    if not NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid name {name!r}: use letters, digits, '.', '_' or '-'")
    if name in existing:
        raise BottleError(f"a bottle named {name!r} already exists")
    planned = _plan_mounts(mounts or [], existing.values())
    prereqs.ensure_smolvm()
    tag = features_.ensure_built(image, installed)
    auth.ensure_logged_in(installed)

    bottle = Bottle(
        name, uuid.uuid4().hex, image, time.time(),
        features=tuple(installed), memory=specs.memory_limit(spec), spec=spec,
        mounts=tuple(m for m, _ in planned), context=context,
    )
    bottle = _bring_up(bottle, [repo for _, repo in planned], tag)
    log.info("%s: created, %s, features %s", name,
             ", ".join(f"{m.repo} at {m.checkout} on {m.path}" for m in bottle.mounts) or "no repo",
             ", ".join(installed) or "none")
    return bottle


def _plan_mounts(requests: list[MountRequest], others) -> list[tuple[Mount, repos.Repo]]:
    """Each requested mount, checked and worked out, with its repo; nothing written."""
    planned: list[tuple[Mount, repos.Repo]] = []
    for request in requests:
        path = request.path.rstrip("/") or request.path
        if not MOUNT_PATH.fullmatch(path):
            raise BottleError(f"invalid mount path {request.path!r}: an absolute path of names, e.g. /workspace")
        for earlier, _ in planned:
            if path == earlier.path or path.startswith(earlier.path + "/") or earlier.path.startswith(path + "/"):
                raise BottleError(f"mounts {earlier.path} and {path} overlap")
        toplevel = repos.toplevel(request.repo)
        repo = repos.Repo(repos.derive_name(toplevel), toplevel)
        start = _start_at(repo, request.checkout)
        served = _parse_remotes(request.remotes)
        _refuse_a_second_writer(repo, served, others)
        planned.append((Mount(path, str(repo.path), start.branch, start.commit, served), repo))
    return planned


def _bring_up(bottle: Bottle, mounted: list[repos.Repo], tag: str) -> Bottle:
    """Create the bottle's machine from `tag`, and set it up; roll it all back on failure."""
    # Never build on a name something else is using: rollback would then be cleaning up after it.
    if smolvm.state(bottle.machine) is not None:
        raise BottleError(f"a machine named {bottle.machine} already exists; choose another name with --name")
    _save(bottle)
    try:
        bottle = _run_machine(bottle, mounted, tag)
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
        for mount in other.mounts:
            shared = ours & {r.push_namespace for r in mount.remotes if r.push}
            directory = git_dir(mount)
            if shared and directory is not None and directory / "objects" == mine:
                raise BottleError(
                    f"bottle {other.name} already pushes into namespace {min(shared)} of {repo.path}; "
                    "each namespace has one writer"
                )


def _run_machine(bottle: Bottle, mounted: list[repos.Repo], tag: str) -> Bottle:
    """Create and start a fresh machine for the bottle from `tag`'s archive, and set it up from scratch."""
    archive = images.archive_of(tag)
    if archive is None:
        raise BottleError(f"{tag} isn't built")
    # Recorded first, so the archive is kept from the moment a machine could have been made from it.
    bottle = replace(bottle, archive=archive.name)
    _save(bottle)
    # The bottle gets stand-ins, never credentials (see auth.py), and no proxy: its flows are decided as they open.
    smolvm.create(
        bottle.machine, str(archive),
        mounts=[smolvm.Mount(repos.objects_dir(repo), mount.objects) for repo, mount in zip(mounted, bottle.mounts)],
        env=_machine_env(bottle.features, bottle.spec.get("permissions", ()), bottle.workdir),
        memory_mib=_memory_mib(bottle.memory), cpus=os.cpu_count() or 1,
    )
    _start_machine(bottle)
    verify_contract(bottle)
    trust_egress_ca(bottle)
    point_at_host(bottle)
    _init_mounts(bottle)
    return bottle


def _start_machine(bottle: Bottle) -> None:
    """Start the bottle's machine, its flows decided by bottled under a token new for this run.

    Its egress CA is made first, for the hosts its features name now: bottled
    terminates TLS for the hosts that CA covers (see daemon.py), and
    trust_egress_ca installs it once the machine is up.
    """
    ca.certificate(bottle.name, auth.intercepted_hosts(bottle.features))
    decider, token = daemon.bind_decider(bottle.name, served(bottle), bottle.features)
    smolvm.start(bottle.machine, decider, token)


def _memory_mib(memory: str | None) -> int:
    """A machine's memory in MiB, from its spec's size (8G, 512M). It takes it as the guest touches it."""
    match = re.fullmatch(r"([1-9][0-9]*)([MG])", (memory or "").strip().upper())
    if match is None:
        raise BottleError(f'a bottle needs its memory, a size in M or G, e.g. "8G" (not {memory!r})')
    return int(match.group(1)) * (1024 if match.group(2) == "G" else 1)


def point_at_host(bottle: Bottle) -> None:
    """Give the bottle bottle.host, at the address whose flows the decider hands to bottled's git service."""
    entry = f"{host.ADDRESS} {host.HOST}"
    script = f"grep -qsx {shlex.quote(entry)} /etc/hosts || echo {shlex.quote(entry)} >> /etc/hosts"
    try:
        _exec(bottle, ["sh", "-c", script], user="root")
    except BottleError as e:
        raise BottleError(f"giving {bottle.machine} bottle.host failed: {e}") from None


def _exec(bottle: Bottle, argv: list[str], user: str | None = None, input: str | None = None) -> str:
    """Run a command in the bottle's running machine and return its output."""
    return smolvm.exec_(bottle.machine, argv, user=user, input=input)


@contextmanager
def throwaway(feature_specs: list[str]):
    """A short-lived bottle with just these features and network access, and no repo.

    Yields a function turning a command into the command line that runs it in
    the throwaway bottle, attached to a terminal. Everything is removed on exit.
    """
    tag = features_.ensure_built(images.BASE, feature_specs)
    name = f"bottle-throwaway-{uuid.uuid4().hex[:12]}"
    log.info("%s: throwaway bottle with %s", name, ", ".join(feature_specs) or "no features")
    try:
        archive = images.archive_of(tag)
        if archive is None:
            raise BottleError(f"{tag} isn't built")
        # No spec to say how much memory: smolvm's own default, for a login command.
        smolvm.create(name, str(archive), mounts=[], env={}, memory_mib=None, cpus=os.cpu_count() or 1)
        # Bound with no features, so no credentials and no egress CA: a login starts from
        # nothing, and mustn't find its own credential attached to what it sends.
        decider, secret = daemon.bind_decider(name)
        smolvm.start(name, decider, secret)
        yield lambda argv: smolvm.exec_command(name, argv, user=USER, tty=True)
    finally:
        for cleanup in (lambda: daemon.release_egress(name), lambda: smolvm.delete(name)):
            try:
                cleanup()
            except Exception as e:
                log.warning("%s: cleaning up: %s", name, e)
                print(f"bottle: cleaning up {name}: {e}", file=sys.stderr)
        log.info("%s: throwaway bottle removed", name)


def git_dir(mount: Mount) -> Path | None:
    """The git dir of a mount's repo, which its remotes are served from; None if it's gone."""
    try:
        return repos.objects_dir(repos.Repo("", Path(mount.repo))).parent
    except BottleError:
        return None


def served(bottle: Bottle) -> tuple[host.Served, ...]:
    """What bottled serves the bottle at bottle.host: each mount's repo and remotes."""
    return tuple(host.Served(git_dir(m), m.remotes, m.at or "") for m in bottle.mounts)


def _machine_env(features: tuple[str, ...] | list[str], permissions: tuple[str, ...] | list[str] = (),
                 workdir: str = HOME) -> dict[str, str]:
    """What a bottle's machine is created with: the stand-ins its features asked for, never credentials;
    its permissions, as BOTTLE_PERMISSIONS, so the tools that use them know to offer them; and where a
    login starts, as BOTTLE_WORKDIR (the base image's profile goes there).

    BOTTLE_PERMISSIONS only says what bottled would allow: bottled checks the
    bottle's spec, not this, so a bottle that sets it itself gains nothing.

    Every exec sees these. SSH sessions only see /etc/environment, so the base
    image's entrypoint mirrors there the ones BOTTLE_MIRROR_ENV names.
    """
    env = (auth.standins_for(features) | ({"BOTTLE_PERMISSIONS": ",".join(permissions)} if permissions else {})
           | {"BOTTLE_WORKDIR": workdir})
    return env | {"BOTTLE_MIRROR_ENV": " ".join(sorted(env))}


def context(bottle: Bottle) -> str:
    """What the agent should know about its bottle, written to ~/BOTTLE.md when its workspace is set up:
    what the bottle is, what its features give it, and nothing about how to work in it.

    Agent-neutral: agent features point their own instruction files at it
    (the claude feature links ~/.claude/CLAUDE.md to it).

    How to work -- branches, pushing, merging -- is its creator's to say, in
    the context it adds; this says what's there.
    """
    lines = [f"# Bottle\n\nYou're `{USER}`, running in a bottle: a sandboxed Linux VM with passwordless sudo.\n"]
    for mount in bottle.mounts:
        at = f"on the `{mount.branch}` branch" if mount.branch else "at a detached commit"
        lines.append(f"- `{mount.path}` is a checkout of `{Path(mount.repo).name}`, {at}.")
        if mount.remotes:
            lines.append("  Its remotes are served by the host:")
            lines += [f"  - `{r.name}`: {_describe_remote(r)}" for r in mount.remotes]
        else:
            lines.append("  It has no remotes.")
    if not bottle.mounts:
        lines.append("- This bottle has no repo.")
    lines.append("- The network works as usual, DNS included. Every connection is decided by\n"
                 "  the host: private addresses are blocked, and a refused one is reset. `bottle.host` is the host.")
    resolved = features_.resolve(list(bottle.features))
    if any(f.credentials for f in resolved):
        lines.append("- A credential the host attaches goes only where its feature allows: a request it refuses\n"
                     "  comes back 403, saying why. That's policy, not a fault: if you need it, tell the user.")
    # What the spec permits it beyond every bottle, by specs.PERMISSIONS' own word on each.
    permissions = bottle.spec.get("permissions", [])
    if permissions:
        lines.append("\n## Permissions\n\nThe host lets this bottle do more than most:\n")
        lines += [f"- {specs.PERMISSIONS[p]}" for p in permissions if p in specs.PERMISSIONS]
    # Each feature's own word on what it gives the agent (customizations.bottle.context).
    told = [(f.id, f.agent_context()) for f in resolved]
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


def _init_mounts(bottle: Bottle) -> None:
    """Check out each mount's branch or commit at its path, borrowing its mounted objects, with its remotes;
    then write the context.

    Each path is made for genie (as root: it may be anywhere), and marked a
    safe directory for git, whoever ends up owning it. No branch tracks
    anything: which remote a branch pulls from is its creator's to decide.
    """
    paths = " ".join(shlex.quote(m.path) for m in bottle.mounts)
    if paths:
        try:
            _exec(bottle, ["sh", "-c", f"install -d -o {USER} -g {USER} {paths}"], user="root")
        except BottleError as e:
            raise BottleError(f"making {paths} failed: {e}") from None
    script = ["set -e"]
    for mount in bottle.mounts:
        path, commit = shlex.quote(mount.path), shlex.quote(mount.commit)
        script += [f"git config --global --add safe.directory {path}"]
        if mount.branch:
            branch = shlex.quote(mount.branch)
            script += [f"git init -q -b {branch} {path}"]
            point_head = f"update-ref refs/heads/{branch} {commit}"
        else:
            script += [f"git init -q {path}"]
            point_head = f"update-ref --no-deref HEAD {commit}"
        script += [
            f"echo {shlex.quote(mount.objects)} > {path}/.git/objects/info/alternates",
            f"git -C {path} {point_head}",
            f"git -C {path} reset -q --hard",
        ]
        names = [r.name for r in mount.remotes]
        script += [f"git -C {path} remote add {name} {shlex.quote(mount.url(name))}" for name in names]
        if ORIGIN in names:
            script += [f"git -C {path} config checkout.defaultRemote {ORIGIN}"]
        if names:
            script += [f"git -C {path} fetch -q --multiple {' '.join(names)}"]
    script += [f"printf '%s' {shlex.quote(context(bottle))} > \"$HOME/{CONTEXT_FILE}\""]
    try:
        _exec(bottle, ["sh", "-c", "\n".join(script)], user=USER)
    except BottleError as e:
        raise BottleError(f"checking out {paths or 'nothing'} failed: {e}") from None


def start(name: str) -> Bottle:
    auth.ensure_logged_in(get(name).features)
    return ensure_running(name)


def stop(name: str) -> None:
    """Stop the bottle's machine, and its token at the decider. Its checkout and changes are kept."""
    bottle = get(name)
    if smolvm.state(bottle.machine) == "running":
        smolvm.stop(bottle.machine)
        log.info("%s: stopped", name)
    daemon.release_egress(bottle.name)


def shutdown() -> tuple[list[str], bool]:
    """Stop every running bottle, then bottled. Returns the stopped bottles and whether bottled was running."""
    stopped = []
    for bottle in load().values():
        if smolvm.state(bottle.machine) == "running":
            smolvm.stop(bottle.machine)
            daemon.release_egress(bottle.name)
            log.info("%s: stopped (shutdown)", bottle.name)
            stopped.append(bottle.name)
    return stopped, daemon.stop()


def ensure_running(name: str) -> Bottle:
    """Start the bottle's machine if it isn't running, bound to the decider."""
    bottle = get(name)
    if bottle.status != "ready":
        raise BottleError(f"{name} is {bottle.status}; remove it with `bottle delete {name}`")
    state = smolvm.state(bottle.machine)
    if state is None:
        raise BottleError(f"{bottle.name}'s machine is gone; `bottle delete {bottle.name}`, then `bottle new` again")
    if state != "running":
        prereqs.ensure_smolvm()
        _start_machine(bottle)
        log.info("%s: started", bottle.name)
        verify_contract(bottle)
        trust_egress_ca(bottle)
        point_at_host(bottle)
    else:
        # Running, so its token is kept: bottled, if it had stopped, binds it again as it starts.
        daemon.start()
    return bottle


def trust_egress_ca(bottle: Bottle) -> None:
    """Make the bottle's trust store hold its egress CA's certificate, and no other.

    Every time a bottle starts, so a bottle made before its CA, or whose
    credential hosts have changed since, trusts the current one, and a bottle
    with no credential hosts trusts none. It's what lets bottled attach a
    credential to the bottle's HTTPS (see ca.py); the key stays on the Mac.
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
        _exec(bottle, ["sh", "-c", script], user="root", input=certificate)
    except BottleError as e:
        raise BottleError(f"installing the egress CA in {bottle.machine} failed: {e}") from None


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
    """Open a login shell in the bottle, at its first mount."""
    _attach(name, ["bash", "-l"], tty=True)


def exec_(name: str, argv: list[str]) -> NoReturn:
    """Run one command in the bottle, at its first mount, and exit with its status.

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
    command = smolvm.exec_command(bottle.machine, argv, user=USER, workdir=bottle.workdir, tty=tty)
    os.execvp(command[0], command)


def _in_bottle(bottle: Bottle, argv: list[str]) -> str:
    return _exec(bottle, argv, user=USER).strip()


def list_all() -> list[tuple[Bottle, str]]:
    """Every bottle with its current state: its machine's, unless setup never finished."""
    result = []
    for bottle in sorted(load().values(), key=lambda b: b.created):
        state = bottle.status if bottle.status != "ready" else (smolvm.state(bottle.machine) or "missing")
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
            "features": sorted(bottle.features), "memory": bottle.memory, "spec": bottle.spec,
            "mounts": [{"path": m.path, "repo": m.repo, "branch": m.branch, "commit": m.commit,
                        "remotes": [r.spec for r in m.remotes]} for m in bottle.mounts],
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
    # The registry says the machine is the bottle's: smolvm has no labels to ask.
    for step, action in (
        ("egress", lambda: daemon.release_egress(bottle.name)),
        ("machine", lambda: smolvm.delete(bottle.machine)),
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
    """Delete images whose features or base have changed since they were built, if no bottle uses one.

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
