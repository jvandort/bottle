# Remote runners

Where bottle would split to run bottles on another machine: register a Linux
host reachable over ssh as a runner, and `bottle new --runner NAME` makes a
bottle there, a drone, while everything that's the user's stays on their own
machine. Nothing here is built yet; this is the design, and the order it's
built in.

## Terms

- **Controller**: the user's machine, where the bottle commands and bottled run.
  The source of truth: the registry, credentials, the repos. (In the code,
  a machine's *driver* is smolvm, which carries its guest's sockets.)
- **Runner**: a machine that runs bottles. `local`, the controller itself, is
  one, and the default; a remote runner is reached over ssh.
- **Drone**: a bottle on a remote runner.
- **Hub**: the repo a bottle's git meets in, whose objects it borrows and
  whose remotes it's served: the user's own repo, or one folio owns (a
  thread's notes). Bottle never creates a hub.
- **Mirror**: a remote runner's `git clone --mirror` of a hub. Its one
  remote is named `hub`.
- **Link**: the one ssh connection between the controller and a remote runner.

## What lives where

| The controller | Every runner |
| --- | --- |
| the registry of bottles and runners | smolvm's machines |
| the catalog: features, specs | images, built and kept on the runner (its own CPU architecture) |
| credentials, and each bottle's credential CA | the decider: a bottle's public connections are decided where it runs |
| hubs, and every push into them | serving a bottle's fetches at `bottle.host`: from the hub locally, from the runner's mirror remotely |
| `git fetch origin` in a hub before an upstream fetch | the HTTP cache, and each bottle's cache CA |
| messages between bottles | (remote) the mirrors, and a runner daemon |

Locally both columns are the same machine, and the code is the same as
remotely wherever it can be. What differs is where a fetch is served from:
the hub itself, which is on the same disk, or a mirror of it.

## The runner interface

Bottle's code reaches machines through one Python interface, `Runner`, and
never runs smolvm itself (only `smolvm.py` does, under the local runner):

- `ensure_ready()`: smolvm installed, `/dev/kvm` usable, and the rest a
  runner needs (see `bottle runner status`).
- Images: look one up by tag, build one from a context the controller sends,
  forget one, remove unused archives.
- Machines: create (from an image, with mounts, env, memory and CPUs),
  start (always bound to the decider), stop, delete, state.
- Exec: run a command and return its output, or the command line that runs
  it attached to a terminal.
- Egress: bind a bottle at the decider (token, policy, which flows to
  redirect), release it.
- `objects(hub)`: the path on the runner whose objects a machine mounts for
  that hub.

`LocalRunner` is today's code, run in-process. `SshRunner` does the same over
the link. `bottles.py` can't tell which it has. Folio still drives bottle
through its CLI and isn't changed: a drone is a bottle like any other.

## The link

bottled holds one ssh connection to each remote runner it needs, using the
user's own ssh setup (`~/.ssh/config`, their keys and agent), as an ssh
ControlMaster. Everything between the two machines goes over it, as channels
inside the one connection, so none of them logs in again:

- **Controller to runner**: runner operations (create, start, build, ...), and
  exec, a shell included, byte-transparent with a TTY when attached.
  smolvm's `exec` always gets `-i`: without it, output is cut at 11 MiB.
- **Runner to controller**: `ssh -R` forwards a Unix socket on the runner,
  `~/.bottle/link.sock` (owner-only, sshd's default), to bottled's. Each
  connection to it is a channel back: a bottle's push, a mirror's refresh,
  a credentialed request, a message. Either side opens channels whenever it
  needs to.

On the runner, a small daemon holds the decider and its tokens, as bottled
does locally. It runs detached, not as a child of the ssh session, so a
dropped link doesn't take it down, and its tokens are on disk. Its code is
this repo's Python, copied over on connect into `~/.bottle/runner/<hash>/`,
named by its content's hash, so a runner never runs code older than the
controller's. Everything bottle keeps on a runner is under `~/.bottle`; smolvm
keeps its own state where it does.

### When the link is down

A runner being off is normal, not an error: it's registered, and unreachable.

- bottled connects when a command needs the runner or a drone there is
  running, backs off to about a minute between attempts, and stops trying
  once no drone there is running.
- A command for an unreachable runner fails at once, saying so and what ssh
  said. `bottle status` shows its drones as `unreachable` and everything
  else as usual. Local bottles never wait on a remote.
- A running drone keeps its public network: the decider is on the runner.
  Whatever needs the controller fails, and the guest sees it fail: fetches,
  pushes, credentialed requests, messages.
- On reconnecting, bottled starts the runner daemon if it's gone (the runner
  rebooted), forwards the socket again, and reads the machines' states again.

## Git

A bottle's git is as it is today (see `bottle/host.py`): its checkout at each
mount's path borrows the hub's objects from a read-only mount at
`/mnt/objects<path>`, and its remotes are at `http://bottle.host/git/...`,
each a view (`upstream`, `heads`, `ns/N`) of the hub, with pushes confined to
a namespace, fast-forward only, no deletes, bottle's hooks, and follow after.

What a runner adds is where fetches are served from.

- **Locally**, from the hub, as today. Before an `upstream` fetch, bottled
  runs `git fetch origin` in the hub.
- **On a remote runner**, from its mirror of the hub. Before serving any
  fetch, the runner brings the mirror up to date: `git fetch --prune hub`,
  every ref, over the link, through git's `ext::` transport and a relay to
  the link's socket, where bottled runs `git upload-pack` in the hub. For an
  `upstream` fetch, the relay asks bottled to run `git fetch origin` in the
  hub first. If the refresh fails, so does the fetch: a drone only ever sees
  the hub's refs as they are when it fetches. The runner then serves the
  view from the mirror, with the same code bottled serves the hub with, and
  the guest, borrowing the mirror's objects, receives little but refs.
- **Pushes** always go to the controller, into the hub, through bottled, with
  every rule above. The mirror catches up at the next fetch.

So the runner is a caching proxy for the hub, never a second source of
truth. Which remotes a bottle may use is checked where the bottle runs, from
what it was bound with. The controller serves a runner's refresh of any hub
with a bottle on that runner, on nobody's behalf.

A mirror is a plain one: refs exactly the hub's, and git's own gc. So a
drone is exposed to gc exactly as a local bottle is (docs/TODO.md: objects a
bottle borrows aren't protected from gc), and its disk stays bounded. A
mirror is kept after its last drone is deleted, as an image is, so the next
drone doesn't start with a full clone. A drone's mount is the mirror's
objects at the same `/mnt/objects<path>`, and a snapshot from one runner
resumes on another: folio repoints a checkout's objects and remotes on resume.

`git fetch origin` before an upstream fetch isn't throttled. Fetches of one
hub share: one asked for while another runs waits for the next, which all
those asked meanwhile share, so each sees origin as it was when it asked. If
origin can't be reached, the fetch is served what the hub has, as today;
that's origin being down, not the link.

A runner's mirror is identified by the hub's key, `<name>-<hash>`, the name
and a hash of the hub's path, as folio keys repos (`folio/folio/repos.py`).

## Egress, credentials and the cache

bottled answers each guest connection in two parts, already two methods:

- **`Gateway.decide(flow)`**: from the destination and the flow's first
  bytes, allow direct, deny, or redirect. It needs only policy, and runs on
  the runner. "Private" addresses are the runner's: a drone can't reach the
  controller's VPN routes or local network, only the runner's, and only as the
  decider allows.
- **`Gateway.serve(flow, decision, ...)`**: be the far end of a redirected
  flow. Serving `bottle.host`'s fetches, and the HTTP cache, run on the
  runner. Pushes, messages, and terminating TLS for a credential's host run
  on the controller: a drone's are carried over the link.

The decider protocol between smolvm and the decider is in `decider.py`'s
docstring. A flow smolvm sends before its ClientHello is complete is
redirected (`OPENING`) wherever it's terminated, which reads the rest.

Each bottle has its own credential CA, as today, made and kept on the
controller, its key never leaving it, constrained to the hosts the bottle's
credentials name. A drone's cached hosts are terminated on its runner, so
each bottle also gets a cache CA, made and kept on its runner, constrained
to its cached hosts less its credential hosts: a host that's both is
terminated on the controller, so the credential is attached. The bottle trusts
both. On a local bottle the two can stay one CA, as today.

## Images

Built as today (`images.py`): buildah in a throwaway builder machine, the
inputs sent in as a tar over exec's stdin and the archive read out over its
stdout. On every runner the controller works out what to build (the tag, its
inputs' hash, the context), and the runner builds it and keeps the archive
in its own store, from images in that store. A remote runner's archives
never come back. A build's context goes over ssh as a tar, and its progress
comes back on the user's terminal. Feature installs choose their downloads by
`dpkg --print-architecture`.

The inputs' hash doesn't include the CPU architecture: a store is one
runner's, of its own architecture, and an archive never moves between
runners, so it can't be taken for another's. Putting it in the hash would
only rebuild every image there is, once, for nothing. If archives ever move
between runners, it goes in then. `bottle build --runner NAME` builds on one.

A paused thread resumes on any runner, whatever its architecture: what an
agent built and kept in its checkout is its own to rebuild.

## Commands

| Command | |
| --- | --- |
| `bottle runner add NAME --ssh DESTINATION` | Register a runner: the ssh destination (an `~/.ssh/config` host, or `user@host`) `--runner NAME` means. Checks it as `status` does, and prepares it: the runner code, and smolvm if missing (asking first: smolvm's latest Linux release, checksum checked, into `~/.bottle/smolvm`) |
| `bottle runner status [NAME]` | Each runner: reachable or not, its OS and architecture, what it's missing (kvm, Python 3.11+, git, smolvm), and its bottles |
| `bottle runner stop NAME` | Stop every bottle on the runner, to shut the machine down. Their disks stay on the runner |
| `bottle runner remove NAME` | Forget a runner with no bottles |
| `bottle new --runner NAME ...` | Make the bottle on that runner; default `local` |

The registry records each bottle's runner. Every other command takes a
bottle's name as today and finds its runner from the registry.

## What has to keep holding

Wherever a bottle runs:

1. Nothing in the guest, root included, reaches the network except as the
   decider allows.
2. The runner's own loopback and link-local addresses are never reachable,
   and private addresses (RFC 1918, ULA, CGNAT) only for an allowed name or
   a credential's host. On a remote runner, "private" means the runner's
   network.
3. Which bottle a connection belongs to comes from the token smolvm was
   started with and the first machine ID seen with it, never from the guest.
   Names from first bytes are the guest's claims: a private address is
   allowed by name only if the name resolves there.
4. A credential never enters the guest, a log, or a remote runner. It's sent
   only over TLS bottled verified, to the host it belongs to, from the controller.
5. Git through `bottle.host` stays as folio's lanes need it, on disk in the
   hub (gitnamespaces(7)): the views, and pushes confined to
   `refs/namespaces/N/`, append-only, one writer per namespace, the hub's
   own hooks off, objects in the hub's own store. A remote runner serves a
   fetch only once its mirror has the hub's refs as they are.
6. The link being down never opens anything: what needs the controller fails.

## Order of work

Each step is pushed as it lands; up to the hardware, it's tested with a
fake ssh (which runs the command locally) and the fake smolvm the tests
already have.

1. Drop the pull-through throttle; concurrent fetches share the next one.
2. The `Runner` interface and `LocalRunner`: `bottles.py`, `images.py`,
   bottled's message delivery and `prereqs.py` go through it. No change in
   behaviour.
3. The registry records each bottle's runner, and `bottle new --runner`
   (only `local` until there are others).
4. Serving split: `bottle.host` fetches served runner-side from a given git
   dir, with the step before an upstream fetch; pushes and messages handed
   to the controller. Local behaviour unchanged.
5. `SshRunner` and the link: the connection and reconnecting, the runner
   daemon, the decider on the runner, the forwarded socket, exec and shells;
   `bottle runner add`, `status` and `remove`, prerequisites checked per
   runner.
6. Images built and kept on remote runners.
7. Mirrors: cloned and refreshed over the link, with `git fetch origin`
   first for upstream; drones mount the mirror's objects.
8. Across runners: stop, delete, `bottle shutdown`, `bottle runner stop`,
   status, messages to drones, the link going down.
9. The HTTP cache on the runner, and the cache CA.
10. On a real x86_64 runner: the smolvm contract test, then a bottle end to
    end. The README after.

## Rejected

- **A copy of the hub on the local runner too**, for one code path and gc
  protection: hard-linking it is free at first, but it grows as the hub
  repacks, and a copy that never removes objects grows forever.
- **Filling a mirror by `git push` over ssh**: a second git path, outside
  bottled. A runner already reaches bottled's git over the link.
- **A guest's fetch writing the mirror**: it's shared by every drone of that
  hub on the runner, and an agent is root in its machine.
- **Bottle owning the hubs** (a bare repo per project, the user's checkout
  a client of it): follow fast-forwards the user's checked-out branches in
  the hub, so the hub is the repo they have open. A hub with a separate
  checkout means two origins fetched separately and two places a branch
  can be ahead. Worth revisiting for a drone of a repo with no checkout on
  the controller.
- **A drone fetching origin itself**: two views of upstream, and a private
  repo's credential would have to reach the runner.
- **A cache CA per runner**: one key would vouch for every bottle's cached
  hosts there.

## Open questions

- **New packs in a running drone**: whether objects a mirror's refresh adds
  show up through smolvm's read-only mount while the machine runs. Git looks
  for new packs when an object is missing, so they should, if the mount
  shows new files. Matters for a mirror's gc and refreshes while drones run.
- **DNS**: smolvm answers guest DNS from the first nameserver in the
  runner's `/etc/resolv.conf`, fixed when a machine starts: the runner's
  resolver, not the controller's.
- **Pausing a runner's bottles**: `bottle runner stop` keeps their disks on
  the runner. Folio's pause, which brings a thread's state back to the
  controller, would be folio's, for a runner that's going away.
- **Watching memory**: having guests give memory back under pressure
  (docs/TODO.md) acts on machines, so it's the runner's. smolvm on Linux
  reclaims guest memory with `MADV_DONTNEED`, so the macOS accounting
  problem (smolvm#1944) doesn't apply there.
