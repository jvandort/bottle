"""Bottles: a sandbox, in its own VM, made from a spec and served the git remotes it's given.

A bottle's record is written before anything is created, so a failure at any
point leaves a record of what may exist. It's kept with the thread it's for, in
its directory in folio's home: <repo key>/<thread>/bottles/<name>.json.
Creation rolls back on failure; delete() removes whatever parts exist, so it
also cleans up a bottle that was left half-made.

Each bottle has:
  - a machine, <name> (see krun.py), with each of its mounts' repos'
    objects mounted read-only, and the network policy it decides its
    connections by (network), which routes some to folio serve: bottle.host,
    which serves each mount's git remotes, ssh to bottles, and its credentials'
    hosts (see services.py, egress.py, host.py)
  - for each mount, a checkout at its path that borrows those objects via
    alternates. Which paths is the bottle's creator's to say; none is special,
    but the first is where a shell or a command starts (genie's home without one).

What it installs, and its memory, come from a spec its creator owns (see
specs.py). Its remotes are the ones it was created with, and nothing is
registered in the repo: what the bottle pushes is for its creator to read.
What's worth keeping before a bottle is deleted is its creator's call too.

A bottle may ssh to other bottles its creator names (ssh: {NAME: BOTTLE}),
and to nothing else that way: NAME, in its /etc/hosts, is an address whose
port 22 folio serve joins to sshd in that bottle, the one that existed when the
grant was made. Its key is let in there and that bottle's host key is known
here, when it's made: so that bottle must exist first, and be made again
with it, or the grant reaches nothing. The two may push into the same
namespaces, which no other two bottles may: they work together.

Objects the bottle uses from the repo aren't protected from `git gc` in the
hub: if the user deletes a branch a bottle checked out and gc later prunes
its commits (after git's grace periods, weeks to months), that checkout
breaks, and the bottle has to be made again.
"""

import fcntl
import logging
import re
import shlex
import sys
import time
import uuid
from pathlib import Path
from dataclasses import asdict, dataclass, field, fields, replace

from bottle import auth, cache, daemon, features as features_, host, images, krun, repos, runners, services, specs
from bottle.errors import BottleError
from bottle.git import git
from bottle.store import bottle_home, folio_home, namespace, read_json, write_json

log = logging.getLogger("bottle.bottles")

OBJECTS_ROOT = "/mnt/objects"   # a mount at /x has its objects at /mnt/objects/x
USER = "genie"
HOME = f"/home/{USER}"
# A mount's path: absolute, of name-like segments, so it's safe in a URL and a shell.
MOUNT_PATH = re.compile(r"(?:/[A-Za-z0-9][A-Za-z0-9._-]*)+")
CONTEXT_FILE = "BOTTLE.md"  # in genie's home
GIT_URL = f"http://{host.HOST}{host.GIT_PATH}"
NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
# The remotes a bottle's repo is served when it's given none, read-only.
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
        return f"{GIT_URL}/{self.at}/{remote}"


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
    """A mount as create is asked for one: the repo, where, from what, served what."""

    path: str
    repo: Path
    checkout: str | None = None  # a branch or commit (default: origin's default branch, else what's checked out)
    remotes: list[str] | None = None  # host.Remote specs (default: origin and host, read-only)


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
    # Its creator's addition to ~/BOTTLE.md.
    context: str = ""
    # The image archive its machine was made from, which is kept while it exists.
    archive: str | None = None
    # The runner its machine is on (see runners.py); records from before runners were the local one's.
    runner: str = runners.LOCAL
    # The bottles it may ssh to.
    ssh: tuple[SshGrant, ...] = ()
    # Its machine's name, as bottle-cli here has it (see machine).
    machine_name: str = ""
    # Which of network()'s policies its machine has (NETWORK the current one); 0 for one from before this was
    # recorded. One behind is given the current one as it's next started.
    network_version: int = 0

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
        """Its machine's name, as bottle-cli lists it: its own, in this home's namespace."""
        return self.machine_name

    @property
    def qualified(self) -> str:
        """Its machine, as bottle-cli on the controller names it: RUNNER/NAME for one on a runner."""
        return self.machine if self.runner == runners.LOCAL else f"{self.runner}/{self.machine}"


# Where a thread's directory has its bottles' records.
RECORDS = "bottles"


def records() -> dict[str, Path]:
    """Each bottle's record, by name: in its thread's directory, <repo key>/<thread>/bottles/. bottle's own
    registry, $BOTTLE_HOME/bottles.json, from before folio kept them, is moved in first."""
    if legacy_registry().exists():
        _move_registry()
    return {p.stem: p for p in sorted(folio_home().glob(f"*/*/{RECORDS}/*.json"))}


def load() -> dict[str, Bottle]:
    known = {f.name for f in fields(Bottle)}
    bottles = {}
    for name, path in records().items():
        entry = read_json(path)
        # Keys a record from an older bottle may have (its driver, say) are left out.
        if entry is not None:
            bottles[name] = Bottle(**{k: v for k, v in entry.items() if k in known})
    return bottles


def legacy_registry() -> Path:
    return bottle_home() / "bottles.json"


def _move_registry() -> None:
    """bottles.json's bottles, each to its thread's directory: the thread whose bottle, or sidecar, it is. One no
    thread has is said, and left to its machine, bottle-cli's. Then bottles.json is gone. Once, whichever process
    gets there first."""
    home = folio_home()
    home.mkdir(parents=True, exist_ok=True)
    with (home / ".registry.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        data = read_json(legacy_registry())
        if data is None:
            return
        owners: dict[str, Path] = {}
        for record in home.glob("*/*/current/thread.json"):
            thread = read_json(record) or {}
            if thread.get("bottle"):
                owners[thread["bottle"]] = owners[f"{thread['bottle']}-sidecar"] = record.parent.parent
        orphans = []
        for name, entry in data.get("bottles", {}).items():
            if name in owners:
                write_json(owners[name] / RECORDS / f"{name}.json", entry)
            else:
                orphans.append(entry.get("machine_name") or f"bottle-{namespace()}{name}")
        legacy_registry().unlink()
    log.info("bottles.json moved into %s; no thread had %s", home, ", ".join(orphans) or "none")
    if orphans:
        print(f"bottle: bottles.json is moved into folio's threads; no thread had the bottles whose machines are "
              f"{', '.join(sorted(orphans))}: they're bottle-cli's now (`bottle machine delete --name MACHINE`)",
              file=sys.stderr)


def get(name: str) -> Bottle:
    bottles = load()
    if name not in bottles:
        known = ", ".join(sorted(bottles)) or "none"
        raise BottleError(f"no bottle named {name!r} (bottles: {known})")
    return bottles[name]


def runner_of(bottle: Bottle) -> runners.Runner:
    """The runner the bottle's machine is on."""
    return runners.get(bottle.runner)


def _save(bottle: Bottle, owner: Path | None = None) -> None:
    """Write the bottle's record where it is, else in thread directory `owner`'s."""
    path = records().get(bottle.name)
    if path is None:
        if owner is None:
            raise BottleError(f"bottle {bottle.name} has no record, and no thread to keep one")
        path = owner / RECORDS / f"{bottle.name}.json"
    write_json(path, asdict(bottle))


def _forget(name: str) -> None:
    path = records().get(name)
    if path is not None:
        path.unlink(missing_ok=True)
        try:
            path.parent.rmdir()   # a thread's bottles/, with none left
        except OSError:
            pass


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
    owner: Path, spec: dict, name: str, mounts: list[MountRequest] | None = None, context: str = "",
    runner: str = runners.LOCAL, ssh: dict[str, str] | None = None,
) -> Bottle:
    """Create a bottle from a spec: its features and memory, from a file its creator owns (see specs.py).

    `name` is the creator's choice, and the bottle's identity: there's no
    default, so a creator never has to find out what it got, and a taken name
    is refused rather than numbered. Each mount is a repo whose objects are
    mounted and whose remotes are served, checked out at its path; without
    any, the bottle has no repo. Nothing is registered in a repo: the bottle's
    creator reads what it pushes, from the namespaces it chose. Its machine is
    on `runner`. It may ssh to each bottle `ssh` gives, by the name given: {name: bottle}. Its record is kept
    in `owner`, its thread's directory.
    """
    on = runners.get(runner)
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
    auth.warn_unset(installed)

    bottle = Bottle(
        name, uuid.uuid4().hex, images.BASE, time.time(),
        features=tuple(installed), memory=specs.memory_limit(spec), spec=spec,
        mounts=tuple(m for m, _ in planned), context=context, runner=runner, ssh=grants,
        machine_name=f"{namespace()}{name}",
    )
    bottle = _bring_up(bottle, [repo for _, repo in planned], tag, owner)
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
            raise BottleError(f"invalid ssh name {name!r}: a host name, e.g. sidecar.host")
        if target not in existing:
            raise BottleError(f"ssh {name}={target}: no bottle named {target!r}")
        if existing[target].status != "ready":
            raise BottleError(f"ssh {name}={target}: {target} is {existing[target].status}")
        grants.append(SshGrant(name, target, existing[target].id, host.ssh_address(index)))
    return tuple(grants)


def _bring_up(bottle: Bottle, mounted: list[repos.Repo], tag: str, owner: Path) -> Bottle:
    """Create the bottle's machine from `tag`, and set it up, its record in `owner`'s; roll it all back on
    failure."""
    # Never build on a name something else is using: rollback would then be cleaning up after it.
    if runner_of(bottle).state(bottle.machine) is not None:
        raise BottleError(f"a machine named {bottle.machine} already exists")
    _save(bottle, owner)
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
    """Where a checkout starts: a branch (the repo's, else origin's), else any commit; by default origin's
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
    archive = images.archive_of(tag, runner.cli)
    if archive is None:
        raise BottleError(f"{tag} isn't built")
    # Recorded first, so the archive is kept from the moment a machine could have been made from it.
    bottle = replace(bottle, archive=archive.name)
    _save(bottle)
    # The bottle gets stand-ins, never credentials (see auth.py), and no proxy: its flows are decided as they open,
    # by the policy its machine is made with.
    runner.create(
        bottle.machine, archive,
        mounts=[krun.Mount(runner.objects(repo), mount.objects) for repo, mount in zip(mounted, bottle.mounts)],
        env=_machine_env(bottle.features, bottle.spec.get("permissions", ()), bottle.workdir),
        memory_mib=_memory_mib(bottle.memory), disk_gib=specs.disk_gib(bottle.spec),
        # What bottle-cli's exec and shell use unless told, and what its ls shows.
        user=USER, workdir=bottle.workdir, labels=specs.labels(bottle.spec), network=network(bottle),
    )
    bottle = replace(bottle, network_version=NETWORK)
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
    """Start the bottle's machine, folio serve answering what it routes there as this bottle's, its credentials
    read now. bottle-cli makes the machine's caching CA as it starts, for its policy's hosts, which trust_egress_ca
    installs once it's up."""
    daemon.bind(bottle.name, bottle.machine, served(bottle), bottle.features)
    runner_of(bottle).start(bottle.machine)


# The version of the policy network() makes: 1, its routes to folio serve's one socket; 2, folio serve's, by service.
NETWORK = 2


def network(bottle: Bottle) -> dict:
    """The policy the bottle's machine decides its connections by (services.network): what its features cache,
    and what it routes to folio serve: bottle.host, ssh to bottles, and its credentials' hosts, logged in or not."""
    return services.network(auth.credential_destinations(bottle.features), cache.cached_for(bottle.features))


def _memory_mib(memory: str | None) -> int:
    """A machine's memory in MiB, from its spec's size (8G, 512M). It takes it as the guest touches it."""
    match = re.fullmatch(r"([1-9][0-9]*)([MG])", (memory or "").strip().upper())
    if match is None:
        raise BottleError(f'a bottle needs its memory, a size in M or G, e.g. "8G" (not {memory!r})')
    return int(match.group(1)) * (1024 if match.group(2) == "G" else 1)


def point_at_host(bottle: Bottle) -> None:
    """Give the bottle bottle.host, at the address whose flows its machine routes to folio serve's git service, and the
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


def git_dir(mount: Mount) -> Path | None:
    """The git dir of a mount's repo, which its remotes are served from; None if it's gone."""
    try:
        return repos.objects_dir(repos.Repo("", Path(mount.repo))).parent
    except BottleError:
        return None


def served(bottle: Bottle) -> tuple[host.Served, ...]:
    """What folio serve serves the bottle at bottle.host: each mount's repo and remotes."""
    return tuple(host.Served(git_dir(m), m.remotes, m.at) for m in bottle.mounts)


def _machine_env(features: tuple[str, ...] | list[str], permissions: tuple[str, ...] | list[str] = (),
                 workdir: str = HOME) -> dict[str, str]:
    """What a bottle's machine is created with: the stand-ins its features asked for, never credentials;
    its permissions, as BOTTLE_PERMISSIONS, so the tools that use them know to offer them; and where a
    login starts, as BOTTLE_WORKDIR (the base image's profile goes there).

    BOTTLE_PERMISSIONS only says what folio serve would allow: folio serve checks the
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
                 f"  bottle to their current directory: `bottle cp {bottle.qualified}:<path> .`")
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
    auth.warn_unset(get(name).features)
    return ensure_running(name)


def ensure_running(name: str) -> Bottle:
    """Start the bottle's machine if it isn't running, folio serve answering it."""
    bottle = get(name)
    if bottle.status != "ready":
        raise BottleError(f"{name} is {bottle.status}; delete its thread to remove it (`folio delete`)")
    runner = runner_of(bottle)
    state = runner.state(bottle.machine)
    if state is None:
        raise BottleError(f"{bottle.name}'s machine is gone; delete its thread (`folio delete`)")
    if state != "running":
        given = bottle.network_version < NETWORK
        if given:
            # Made with another policy than this one: given it now.
            runner.configure(bottle.machine, network(bottle))
            bottle = replace(bottle, network_version=NETWORK)
            _save(bottle)
        _start_machine(bottle)
        log.info("%s: started", bottle.name)
        if given:
            # What's set up in it for its policy, again; one made with this policy has all it needs on its
            # disk, as bottle-cli's start alone would find it.
            verify_contract(bottle)
            trust_egress_ca(bottle)
            point_at_host(bottle)
    else:
        # Running: folio serve, if it had stopped, binds it again as it starts.
        daemon.start()
    return bottle


def trust_egress_ca(bottle: Bottle) -> None:
    """Make the bottle's trust store hold its machine's CA's certificate.

    The CA bottle-cli takes TLS off with, where the bottle runs, for the hosts
    its policy caches over https and routes by name (bottle-cli ca): what lets
    it answer cached hosts and give folio serve credentials' requests. One with
    no such hosts trusts none.
    """
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


def exec_command(name: str, argv: list[str], tty: bool = False) -> list[str]:
    """The command line, for this machine, that runs `argv` in the bottle as genie, at its first mount: started if
    it's stopped, logged in first to what its features need."""
    auth.warn_unset(get(name).features)
    bottle = ensure_running(name)
    return runner_of(bottle).exec_command(bottle.machine, argv, user=USER, workdir=bottle.workdir, tty=tty)


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
    """Every bottle's state and what it was made from, or just those `names`.

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
            # Its machine, as bottle-cli here names it (`bottle shell MACHINE`).
            "machine": bottle.qualified,
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
        ("egress", lambda: daemon.release_egress(bottle.name)),
        ("machine", lambda: runner.delete(bottle.machine)),
    ):
        try:
            action()
        except Exception as e:  # keep going: clean up every part we can
            failures.append(f"{step}: {e}")
    if failures:
        log.warning("%s: deleting left parts behind: %s", name, "; ".join(failures))
        _save(replace(bottle, status="broken"))
        raise BottleError(f"couldn't fully delete {name} ({'; '.join(failures)}); try again, and what's left is deleted")
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
