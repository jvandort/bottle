"""Bottles: a sandbox, in its own VM, made from a spec and served the git remotes it's given.

The registry at $BOTTLE_HOME/bottles.json is written before anything is
created, so a failure at any point leaves a record of what may exist.
Creation rolls back on failure; delete() removes whatever parts exist, so it
also cleans up a bottle that was left half-made.

Each bottle has:
  - a machine, <name> (see krun.py), with each of its mounts' repos'
    objects mounted read-only, and the network policy it decides its
    connections by (network), which routes some to bottled: bottle.host,
    which serves each mount's git remotes, ssh to bottles, and its credentials'
    hosts (see services.py, egress.py, host.py)
  - for each mount, a checkout at its path that borrows those objects via
    alternates. Which paths is the bottle's creator's to say; none is special,
    but the first is where a shell or a command starts (genie's home without one).

What it installs, and its memory, come from a spec its creator owns (see
specs.py). Its remotes are the ones it was created with, and nothing is
registered in the repo: what the bottle pushes is for its creator to read.
What's worth keeping before a bottle is deleted is its creator's call too.

A bottle may ssh to other bottles its creator names (`--ssh NAME=BOTTLE`),
and to nothing else that way: NAME, in its /etc/hosts, is an address whose
port 22 bottled joins to sshd in that bottle, the one that existed when the
grant was made. Its key is let in there and that bottle's host key is known
here, when it's made: so that bottle must exist first, and be made again
with it, or the grant reaches nothing. The two may push into the same
namespaces, which no other two bottles may: they work together.

Objects the bottle uses from the repo aren't protected from `git gc` in the
hub: if the user deletes a branch a bottle checked out and gc later prunes
its commits (after git's grace periods, weeks to months), that checkout
breaks, and the bottle has to be made again.
"""

import logging
import os
import re
import shlex
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
from contextlib import contextmanager
from pathlib import Path
from dataclasses import asdict, dataclass, field, fields, replace
from typing import NoReturn

from bottle import auth, cache, daemon, features as features_, host, images, repos, runners, services, specs
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
# starts. Documented in containers/features/base/install.sh; keep them in sync.
CONTRACT = (
    ("bottle-entrypoint is installed", "test -x /usr/local/bin/bottle-entrypoint"),
    ("the genie user has UID 1000", '[ "$(id -un)" = genie ] && [ "$(id -u)" = 1000 ]'),
    ("genie has passwordless sudo", "sudo -n true"),
    ("git is installed", "command -v git"),
    ("sshd is installed", "test -x /usr/sbin/sshd"),
    ("update-ca-certificates is installed", "test -x /usr/sbin/update-ca-certificates"),
)
# Where bottled's egress CA was in a bottle's trust store, before its machine's own took its place.
CA_CERTIFICATE = "/usr/local/share/ca-certificates/bottle-egress.crt"
# Where a bottle has its machine's CA, which bottle-cli where it runs takes TLS off with (bottle-cli ca), in the
# trust store whose bundle the base image points NODE_EXTRA_CA_CERTS at, for Node tools.
CACHE_CERTIFICATE = "/usr/local/share/ca-certificates/bottle-egress-cache.crt"


@dataclass(frozen=True)
class Mount:
    """A repo in the bottle: its objects mounted read-only, checked out at `path`
    borrowing them, and served the remotes it was given."""

    path: str  # where it's checked out, absolute
    repo: str  # its toplevel on the controller: the hub, whose objects are mounted
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
class SshGrant:
    """A bottle this one may ssh to: as `name`, at `address`, which is bottle `bottle` with id `id` and no other."""

    name: str
    bottle: str
    id: str
    address: str


# A name a grant gives: a host name, lower case.
SSH_NAME = re.compile(r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)*")


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
    # The runner its machine is on (see runners.py); records from before runners were the local one's.
    runner: str = runners.LOCAL
    # The bottles it may ssh to (`--ssh`).
    ssh: tuple[SshGrant, ...] = ()
    # Its machine's name, as bottle-cli has it; "" for a bottle made before it was recorded (see machine).
    machine_name: str = ""
    # Whether its machine has its network policy (see network); one made before machines had their own is given
    # it as it's next started.
    networked: bool = False

    def __post_init__(self) -> None:
        object.__setattr__(self, "features", tuple(self.features))  # a list, when read from JSON
        object.__setattr__(self, "mounts", tuple(  # dicts, when read from JSON
            Mount(**m) if isinstance(m, dict) else m for m in self.mounts))
        object.__setattr__(self, "ssh", tuple(SshGrant(**g) if isinstance(g, dict) else g for g in self.ssh))

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
        # Its own name (in this home's namespace), as bottle-cli lists it; one made before machines had that,
        # bottle- and it.
        return self.machine_name or f"bottle-{namespace()}{self.name}"


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


def runner_of(bottle: Bottle) -> runners.Runner:
    """The runner the bottle's machine is on."""
    return runners.get(bottle.runner)


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


# What a flow to a bottle it may ssh to is joined to, there: sshd, for that one connection.
SSHD = "mkdir -p /run/sshd && exec /usr/sbin/sshd -i -e"


def sshd_command(name: str, address: str) -> list[str]:
    """The command line, for this machine, that runs sshd for one connection in the bottle `name` may ssh to at
    `address`: the one granted it, if it's still there."""
    bottle = get(name)
    grant = next((g for g in bottle.ssh if g.address == address), None)
    if grant is None:
        raise BottleError(f"{name} may ssh to nothing at {address}")
    there = load().get(grant.bottle)
    if there is None or there.id != grant.id:
        raise BottleError(f"bottle {grant.bottle}, which {name} may ssh to as {grant.name}, is gone")
    runner = runner_of(there)
    if runner.state(there.machine) != "running":
        raise BottleError(f"bottle {grant.bottle}, which {name} may ssh to as {grant.name}, isn't running")
    return runner.exec_command(there.machine, ["sh", "-c", SSHD], user="root")


def create(
    spec: dict, name: str, mounts: list[MountRequest] | None = None, context: str = "",
    runner: str = runners.LOCAL, ssh: dict[str, str] | None = None,
) -> Bottle:
    """Create a bottle from a spec: its features and memory, from a file its creator owns (see specs.py).

    `name` is the creator's choice, and the bottle's identity: there's no
    default, so a creator never has to find out what it got, and a taken name
    is refused rather than numbered. Each mount is a repo whose objects are
    mounted and whose remotes are served, checked out at its path; without
    any, the bottle has no repo. Nothing is registered in a repo: the bottle's
    creator reads what it pushes, from the namespaces it chose. Its machine is
    on `runner`. It may ssh to each bottle `ssh` gives, by the name given: {name: bottle}.
    """
    on = runners.get(runner)
    # Ready first: what it's asked next is its bottle-cli's to answer, as this repo's sources are now.
    on.ensure_ready()
    spec = specs.canonical(spec, on.memory_mib(), "this machine" if runner == runners.LOCAL else f"runner {runner}")
    installed = [f.spec for f in features_.resolve(list(spec["features"]))]
    existing = load()
    if not NAME_PATTERN.fullmatch(name):
        raise BottleError(f"invalid name {name!r}: use letters, digits, '.', '_' or '-'")
    if name in existing:
        raise BottleError(f"a bottle named {name!r} already exists")
    grants = _plan_ssh(ssh or {}, existing)
    # A bottle it may ssh to is one it works with: their lanes may be shared.
    planned = _plan_mounts(mounts or [], [b for b in existing.values() if b.id not in {g.id for g in grants}])
    tag = features_.ensure_built(installed, on)
    auth.ensure_logged_in(installed)

    bottle = Bottle(
        name, uuid.uuid4().hex, images.BASE, time.time(),
        features=tuple(installed), memory=specs.memory_limit(spec), spec=spec,
        mounts=tuple(m for m, _ in planned), context=context, runner=runner, ssh=grants,
        machine_name=f"{namespace()}{name}",
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


def _plan_ssh(ssh: dict[str, str], existing: dict[str, Bottle]) -> tuple[SshGrant, ...]:
    """Each bottle the new one may ssh to, checked: a bottle that exists and is ready, under a host name."""
    if len(ssh) > host.MAX_SSH:
        raise BottleError(f"a bottle may ssh to at most {host.MAX_SSH} others")
    grants = []
    for index, (name, target) in enumerate(ssh.items()):
        if not SSH_NAME.fullmatch(name) or name == host.HOST:
            raise BottleError(f"invalid --ssh name {name!r}: a host name, e.g. sidecar.host")
        if target not in existing:
            raise BottleError(f"--ssh {name}={target}: no bottle named {target!r}")
        if existing[target].status != "ready":
            raise BottleError(f"--ssh {name}={target}: {target} is {existing[target].status}")
        grants.append(SshGrant(name, target, existing[target].id, host.ssh_address(index)))
    return tuple(grants)


def _bring_up(bottle: Bottle, mounted: list[repos.Repo], tag: str) -> Bottle:
    """Create the bottle's machine from `tag`, and set it up; roll it all back on failure."""
    # Never build on a name something else is using: rollback would then be cleaning up after it.
    if runner_of(bottle).state(bottle.machine) is not None:
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
    """Refuse a namespace another bottle already pushes into, in the same repo: each has one writer, but
    for the bottles a new one may ssh to, which `others` leaves out: they work together, and as pushes
    can't rewrite, a racing one is refused rather than lost."""
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
    runner = runner_of(bottle)
    archive = runner.archive(tag)
    if archive is None:
        raise BottleError(f"{tag} isn't built")
    # Recorded first, so the archive is kept from the moment a machine could have been made from it.
    bottle = replace(bottle, archive=archive.name)
    _save(bottle)
    # The bottle gets stand-ins, never credentials (see auth.py), and no proxy: its flows are decided as they open,
    # by the policy its machine is made with.
    runner.create(
        bottle.machine, archive,
        mounts=[runners.Mount(runner.objects(repo), mount.objects) for repo, mount in zip(mounted, bottle.mounts)],
        env=_machine_env(bottle.features, bottle.spec.get("permissions", ()), bottle.workdir),
        memory_mib=_memory_mib(bottle.memory), disk_gib=specs.disk_gib(bottle.spec),
        # What bottle-cli's exec and shell use unless told, and what its ls shows.
        user=USER, workdir=bottle.workdir, labels=specs.labels(bottle.spec), network=network(bottle),
    )
    bottle = replace(bottle, networked=True)
    _save(bottle)
    _start_machine(bottle)
    verify_contract(bottle)
    trust_egress_ca(bottle)
    point_at_host(bottle)
    _init_mounts(bottle)
    for grant in bottle.ssh:
        _let_in(bottle, grant)
    return bottle


def _let_in(bottle: Bottle, grant: SshGrant) -> None:
    """Let the bottle ssh to the one it's granted, as grant.name: its key let in there, that one's host key known
    here."""
    there = ensure_running(grant.bottle)
    key = _in_bottle(bottle, ["sh", "-c", "install -d -m 700 ~/.ssh && { test -f ~/.ssh/id_ed25519 || ssh-keygen -q "
                                          f"-t ed25519 -N '' -f ~/.ssh/id_ed25519 -C {USER}@{bottle.name}; }} && "
                                          "cat ~/.ssh/id_ed25519.pub"])
    host_key = _exec(there, ["sh", "-c", "{ test -f /etc/ssh/ssh_host_ed25519_key || ssh-keygen -q -t ed25519 -N '' "
                                         "-f /etc/ssh/ssh_host_ed25519_key; } && cut -d' ' -f1,2 "
                                         "/etc/ssh/ssh_host_ed25519_key.pub"], user="root").strip()
    # Modes set, not left to a umask: commands run in a machine get 0000, and sshd refuses keys others can write.
    _exec(there, ["sh", "-c", "install -d -m 700 ~/.ssh && read -r key && touch ~/.ssh/authorized_keys && "
                              "chmod 600 ~/.ssh/authorized_keys && "
                              "{ grep -qxF \"$key\" ~/.ssh/authorized_keys || echo \"$key\" >> ~/.ssh/authorized_keys; }"],
          user=USER, input=key + "\n")
    _exec(bottle, ["sh", "-c", "install -d -m 700 ~/.ssh && touch ~/.ssh/known_hosts && chmod 600 ~/.ssh/known_hosts && "
                               f"sed -i '/^{re.escape(grant.name)} /d' ~/.ssh/known_hosts && cat >> ~/.ssh/known_hosts"],
          user=USER, input=f"{grant.name} {host_key}\n")


def _start_machine(bottle: Bottle) -> None:
    """Start the bottle's machine, bottled answering what it routes there as this bottle's, its credentials read
    now. bottle-cli makes the machine's caching CA as it starts, for its policy's hosts, which trust_egress_ca
    installs once it's up."""
    runner_of(bottle).start(bottle.machine, bottle.name, served(bottle), bottle.features)


def network(bottle: Bottle) -> dict:
    """The policy the bottle's machine decides its connections by (services.network): what its features cache,
    and what it routes to bottled: bottle.host, ssh to bottles, and its credentials' hosts, logged in or not."""
    return services.network(auth.credential_destinations(bottle.features), cache.cached_for(bottle.features))


def _memory_mib(memory: str | None) -> int:
    """A machine's memory in MiB, from its spec's size (8G, 512M). It takes it as the guest touches it."""
    match = re.fullmatch(r"([1-9][0-9]*)([MG])", (memory or "").strip().upper())
    if match is None:
        raise BottleError(f'a bottle needs its memory, a size in M or G, e.g. "8G" (not {memory!r})')
    return int(match.group(1)) * (1024 if match.group(2) == "G" else 1)


def point_at_host(bottle: Bottle) -> None:
    """Give the bottle bottle.host, at the address whose flows its machine routes to bottled's git service, and the
    name of each bottle it may ssh to, at its address."""
    entries = [f"{host.ADDRESS} {host.HOST}", *(f"{g.address} {g.name}" for g in bottle.ssh)]
    script = "; ".join(f"grep -qsx {shlex.quote(e)} /etc/hosts || echo {shlex.quote(e)} >> /etc/hosts" for e in entries)
    try:
        _exec(bottle, ["sh", "-c", script], user="root")
    except BottleError as e:
        raise BottleError(f"giving {bottle.machine} bottle.host failed: {e}") from None


def _exec(bottle: Bottle, argv: list[str], user: str | None = None, input: str | None = None) -> str:
    """Run a command in the bottle's running machine and return its output."""
    return runner_of(bottle).exec_(bottle.machine, argv, user=user, input=input)


@contextmanager
def throwaway(feature_specs: list[str]):
    """A short-lived bottle with just these features and network access, and no repo.

    Yields a function turning a command into the command line that runs it in
    the throwaway bottle, attached to a terminal. Everything is removed on exit.
    """
    tag = features_.ensure_built(feature_specs)
    name = f"bottle-throwaway-{uuid.uuid4().hex[:12]}"
    log.info("%s: throwaway bottle with %s", name, ", ".join(feature_specs) or "no features")
    runner = runners.get()
    try:
        archive = runner.archive(tag)
        if archive is None:
            raise BottleError(f"{tag} isn't built")
        # No spec to say how much memory: bottle-cli's own default, for a login command.
        runner.create(name, archive, mounts=[], env={}, memory_mib=None)
        # Bound with no features, so no credentials and no egress CA: a login starts from
        # nothing, and mustn't find its own credential attached to what it sends.
        runner.start(name, name)
        yield lambda argv: runner.exec_command(name, argv, user=USER, tty=True)
    finally:
        for cleanup in (lambda: runner.release(name), lambda: runner.delete(name)):
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
    lines = [f"# Bottle\n\nYou're `{USER}`, running in bottle `{bottle.name}`: a sandboxed Linux VM with passwordless "
             "sudo.\n"]
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
    for grant in bottle.ssh:
        lines.append(f"- `ssh {grant.name}` reaches bottle `{grant.bottle}`, as `{USER}`: a shell, a command, "
                     "scp, git over ssh.")
    lines.append("- The network works as usual, DNS included. Every connection is decided by\n"
                 "  the host: private addresses are blocked, and a refused one is reset. `bottle.host` is the host.")
    lines.append(f"- To hand the user a file or a directory, give them the command that copies it out of this\n"
                 f"  bottle to their current directory: `bottle cp {bottle.name}:<path> .`")
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
    """Stop the bottle's machine, and bottled's answering it. Its checkout and changes are kept."""
    bottle = get(name)
    runner = runner_of(bottle)
    if runner.state(bottle.machine) == "running":
        runner.stop(bottle.machine)
        log.info("%s: stopped", name)
    runner.release(bottle.name)


def shutdown() -> tuple[list[str], bool]:
    """Stop every running bottle, then bottled. Returns the stopped bottles and whether bottled was running.

    Bottles on a runner that can't be reached are left as they are: see stop_all.
    """
    stopped, _ = stop_all()
    return stopped, daemon.stop()


def stop_all(runner: str | None = None) -> tuple[list[str], dict[str, str]]:
    """Stop every running bottle, or every one on `runner`. Returns those stopped, and the runners that
    couldn't be reached, with why: their bottles are left as they are."""
    stopped: list[str] = []
    unreachable: dict[str, str] = {}
    for bottle in load().values():
        if runner is not None and bottle.runner != runner:
            continue
        if bottle.runner in unreachable or runners.is_detached(bottle.runner):
            continue  # a detached runner runs nothing
        on = runner_of(bottle)
        try:
            if on.state(bottle.machine) == "running":
                on.stop(bottle.machine)
                on.release(bottle.name)
                log.info("%s: stopped", bottle.name)
                stopped.append(bottle.name)
        except BottleError as e:
            if bottle.runner == runners.LOCAL:
                raise
            unreachable[bottle.runner] = str(e)
    return stopped, unreachable


def ensure_running(name: str) -> Bottle:
    """Start the bottle's machine if it isn't running, bottled answering it."""
    bottle = get(name)
    if bottle.status != "ready":
        raise BottleError(f"{name} is {bottle.status}; remove it with `bottle delete {name}`")
    runner = runner_of(bottle)
    state = runner.state(bottle.machine)
    if state is None:
        raise BottleError(f"{bottle.name}'s machine is gone; `bottle delete {bottle.name}`, then `bottle new` again")
    if state != "running":
        runner.ensure_ready()
        bottle = _named_machine(bottle, runner)
        given = not bottle.networked
        if given:
            # Made before machines had their own policy: given it now.
            runner.configure(bottle.machine, network(bottle))
            bottle = replace(bottle, networked=True)
            _save(bottle)
        _start_machine(bottle)
        log.info("%s: started", bottle.name)
        if given:
            # What was set up in it for the decider is set up again for its policy; one made with its policy has
            # all it needs on its disk, as bottle-cli's start alone would find it.
            verify_contract(bottle)
            trust_egress_ca(bottle)
            point_at_host(bottle)
    else:
        # Running: bottled, if it had stopped, binds it again as it starts.
        runner.ensure_deciding()
    return bottle


def _named_machine(bottle: Bottle, runner: runners.Runner) -> Bottle:
    """`bottle`, its stopped machine renamed for it if it was made with bottle-'s prefix (see Bottle.machine)."""
    if bottle.machine_name:
        return bottle
    named = replace(bottle, machine_name=f"{namespace()}{bottle.name}")
    try:
        runner.rename(bottle.machine, named.machine)
    except BottleError as e:
        log.warning("%s: its machine keeps its name %s (%s)", bottle.name, bottle.machine, e)
        return bottle
    _save(named)
    log.info("%s: its machine %s is %s now", bottle.name, bottle.machine, named.machine)
    return named


def trust_egress_ca(bottle: Bottle) -> None:
    """Make the bottle's trust store hold its machine's CA's certificate, and no other of bottle's.

    The CA bottle-cli takes TLS off with, where the bottle runs, for the hosts
    its policy caches over https and routes by name (bottle-cli ca): what lets
    it answer cached hosts and give bottled credentials' requests. Each time
    a bottle starts, so one made before, or given its policy since, trusts the
    current one; one with no such hosts trusts none. bottled's own egress CA,
    from before, is taken out.
    """
    _trust(bottle, CA_CERTIFICATE, None)
    _trust(bottle, CACHE_CERTIFICATE, runner_of(bottle).cache_certificate(bottle.machine, []))


def _trust(bottle: Bottle, path: str, certificate: str | None) -> None:
    """Have the bottle's trust store hold `certificate` at `path`, or nothing there if it's None."""
    if certificate is None:
        script = f"if [ -e {path} ]; then rm {path} && update-ca-certificates --fresh >/dev/null; fi"
    else:
        script = (
            f"cat > {path}.new && "
            f"if cmp -s {path}.new {path}; then rm {path}.new; "
            f"else mv {path}.new {path} && update-ca-certificates --fresh >/dev/null; fi"
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


# In a bottle, whatever $1 names, as a tar of it alone: its last name, and what's under it.
_PACK = 'p=$(realpath -e -- "$1") && exec tar -C "$(dirname -- "$p")" -cf - -- "$(basename -- "$p")"'
# In a bottle, the tar on stdin, of one thing, as cp -r would put it: into $1 if it's a directory, else as $1;
# unpacked beside it first, so a copy that fails changes nothing. Says where it went.
_UNPACK = r"""umask 022
d=$1
if [ -d "$d" ]; then into=$d; else into=$(dirname -- "$d"); fi
s=$(mktemp -d -- "$into/.bottle-cp-XXXXXX") || exit 1
trap 'rm -rf -- "$s"' EXIT
tar -xf - -C "$s" --no-same-owner || exit 1
n=$(ls -A -- "$s")
if [ -z "$n" ] || [ "$(printf '%s\n' "$n" | wc -l)" -ne 1 ]; then echo "it sent nothing" >&2; exit 1; fi
if [ -d "$d" ]; then t=${d%/}/$n; else t=$d; fi
if [ -d "$t" ] && [ ! -L "$t" ]; then echo "$t is a directory already: copy into it, or somewhere new" >&2; exit 1; fi
if [ -d "$s/$n" ] && [ ! -L "$s/$n" ] && { [ -e "$t" ] || [ -L "$t" ]; }; then
  echo "$t exists, and what's copied is a directory" >&2; exit 1
fi
mv -fT -- "$s/$n" "$t" && printf '%s\n' "$t"
"""


@dataclass(frozen=True)
class CopySide:
    """One side of a copy: a path in a bottle (relative to its first mount), or, with no bottle, here."""

    bottle: str | None
    path: str

    @classmethod
    def parse(cls, arg: str) -> "CopySide":
        """BOTTLE:PATH, or a path here: as scp has it, a colon before any '/' names a bottle (./a:b doesn't)."""
        name, colon, path = arg.partition(":")
        if colon and NAME_PATTERN.fullmatch(name):
            return cls(name, path or ".")
        return cls(None, arg)

    def __str__(self) -> str:
        return f"{self.bottle}:{self.path}" if self.bottle else self.path


def copy(src: CopySide, dst: CopySide) -> tuple[str, list[str]]:
    """Copy `src`, a file or a directory, to `dst`, as cp -r would: into `dst` if it's a directory, else as
    it. Either side, or both, in a bottle, which is started if stopped. Returns where it went, and what was
    left out of it, saying why.

    It goes as a tar, by way of this machine. Unpacked here, it's data: nothing
    in it lands outside what it names, and a link out of it, or a device, is
    left out. Unpacked in a bottle, it's tar's, as `genie` there. Either way
    nothing is there until all of it is: a copy that fails leaves `dst` as it was.
    """
    if src.bottle is None and dst.bottle is None:
        raise BottleError("bottle cp copies into or out of a bottle: name one, BOTTLE:PATH")
    if src.bottle is not None and src.path.rstrip("/") == "":
        raise BottleError("bottle cp copies a file or a directory, not /")
    if src.bottle is None:
        return _copy_in(Path(src.path), dst), []
    pack = _copy_command(src.bottle, ["sh", "-c", _PACK, "sh", src.path])
    if dst.bottle is None:
        return _copy_out(src, pack, Path(dst.path))
    unpack = _copy_command(dst.bottle, ["sh", "-c", _UNPACK, "sh", dst.path])
    # stdin stays open until it ends: an exec's stdin closing early may end it early.
    with subprocess.Popen(pack, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as sending:
        with subprocess.Popen(unpack, stdin=sending.stdout, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as taking:
            sending.stdout.close()
            out, taken = taking.communicate()
        sending.stdin.close()
        sent = sending.stderr.read()
    if sending.returncode != 0:
        raise BottleError(f"couldn't copy {src}: {_said(sent) or 'it sent nothing'}")
    if taking.returncode != 0:
        raise BottleError(f"couldn't copy {src} to {dst}: {_said(taken) or 'it failed'}")
    return f"{dst.bottle}:{out.decode().strip()}", []


def _copy_command(name: str, argv: list[str]) -> list[str]:
    """The command line that runs `argv` in the bottle as `genie`, at its first mount, started if stopped."""
    auth.ensure_logged_in(get(name).features)
    bottle = ensure_running(name)
    return runner_of(bottle).exec_command(bottle.machine, argv, user=USER, workdir=bottle.workdir)


def _said(stderr: bytes) -> str:
    return stderr.decode(errors="replace").strip()


def _copy_out(src: CopySide, pack: list[str], dst: Path) -> tuple[str, list[str]]:
    into = dst.is_dir()
    with tempfile.TemporaryDirectory(dir=dst if into else dst.parent, prefix=".bottle-cp-") as scratch:
        with subprocess.Popen(pack, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
            names: set[str] = set()
            left_out: list[str] = []
            try:
                with tarfile.open(fileobj=process.stdout, mode="r|") as tar:
                    for member in tar:
                        names.add(member.name.split("/")[0])
                        try:
                            tar.extract(member, scratch, filter="data")
                        except tarfile.FilterError as e:
                            left_out.append(f"{member.name}: {e}")
            except tarfile.TarError:
                names = set()
            process.stdin.close()
            error = _said(process.stderr.read())
        if process.returncode != 0 or len(names) != 1:
            raise BottleError(f"couldn't copy {src}: {error or 'it sent nothing'}")
        [base] = names
        target = dst / base if into else dst
        copied = Path(scratch) / base
        if target.is_dir() and not target.is_symlink():
            raise BottleError(f"{target} is a directory already: copy into it, or somewhere new")
        if copied.is_dir() and target.exists():
            raise BottleError(f"{target} exists, and {src} is a directory")
        copied.replace(target)
    return str(target), left_out


def _copy_in(src: Path, dst: CopySide) -> str:
    if not src.exists() and not src.is_symlink():
        raise BottleError(f"couldn't copy {src}: there's no such file or directory")
    base = src.resolve().name if src.name in ("", ".", "..") else src.name
    unpack = _copy_command(dst.bottle, ["sh", "-c", _UNPACK, "sh", dst.path])
    with subprocess.Popen(unpack, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
        try:
            with tarfile.open(fileobj=process.stdin, mode="w|") as tar:
                tar.add(src, arcname=base)
            process.stdin.close()
        except BrokenPipeError:
            pass   # it stopped taking it: what it says is why
        out, error = process.stdout.read(), process.stderr.read()
    if process.returncode != 0:
        raise BottleError(f"couldn't copy {src} to {dst}: {_said(error) or 'it failed'}")
    return f"{dst.bottle}:{out.decode().strip()}"


def _attach(name: str, argv: list[str], tty: bool) -> NoReturn:
    """Replace this process with `argv` running in the bottle, starting it if stopped."""
    auth.ensure_logged_in(get(name).features)
    bottle = ensure_running(name)
    command = runner_of(bottle).exec_command(bottle.machine, argv, user=USER, workdir=bottle.workdir, tty=tty)
    os.execvp(command[0], command)


def _in_bottle(bottle: Bottle, argv: list[str]) -> str:
    return _exec(bottle, argv, user=USER).strip()


# The state of a bottle on a runner that can't be reached; and on one detached, which isn't asked.
UNREACHABLE = "unreachable"
DETACHED = "detached"


def list_all() -> list[tuple[Bottle, str]]:
    """Every bottle with its current state: its machine's, unless setup never finished, or its runner can't be
    reached or is detached."""
    result = []
    # A runner that couldn't be reached is asked once, not once a bottle.
    unreachable: set[str] = set()
    for bottle in sorted(load().values(), key=lambda b: b.created):
        if bottle.status != "ready":
            state = bottle.status
        elif bottle.runner in unreachable:
            state = UNREACHABLE
        elif runners.is_detached(bottle.runner):
            state = DETACHED
        else:
            try:
                state = runner_of(bottle).state(bottle.machine) or "missing"
            except BottleError:
                if bottle.runner == runners.LOCAL:
                    raise
                unreachable.add(bottle.runner)
                state = UNREACHABLE
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
            "name": bottle.name, "state": state, "created": bottle.created, "runner": bottle.runner,
            "features": sorted(bottle.features), "memory": bottle.memory, "spec": bottle.spec,
            "ssh": {g.name: g.bottle for g in bottle.ssh},
            # Where its objects and remotes are, as a checkout restored into it has to point.
            "mounts": [{"path": m.path, "repo": m.repo, "branch": m.branch, "commit": m.commit,
                        "remotes": [r.spec for r in m.remotes], "objects": m.objects,
                        "urls": {r.name: m.url(r.name) for r in m.remotes}} for m in bottle.mounts],
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
    # The registry says the machine is the bottle's: a machine has no labels to ask.
    runner = runner_of(bottle)
    for step, action in (
        ("egress", lambda: runner.release(bottle.name)),
        ("machine", lambda: runner.delete(bottle.machine)),
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
    _remove_stale_images(runner)


def _remove_stale_images(runner: runners.Runner) -> None:
    """Delete images whose features or base have changed since they were built, if no bottle uses one.

    An image that's still current is kept, even with nothing using it: it's a
    cache, and the next bottle with those features starts without a build.
    Best effort: the bottle is already gone.
    """
    try:
        features_.remove_stale(runner)
    except BottleError as e:
        print(f"bottle: couldn't remove stale images: {e}", file=sys.stderr)


def _describe(failure: BaseException) -> str:
    return "interrupted" if isinstance(failure, KeyboardInterrupt) else str(failure)
