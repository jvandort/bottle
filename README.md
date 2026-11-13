# Bottle

*The perfect home for a genie.*

Bottle manages sandboxed Linux VMs for coding agents, on macOS, or on a Linux machine of yours
over ssh. Let the agent run with full permissions inside. Take back only the commits you want.

- **A real VM per bottle.** Its own kernel, via [libkrun](https://github.com/containers/libkrun),
  with everything it needs already installed. Starts in well under a second, and bottles on the
  same image share it on disk and in your Mac's memory.
- **Your repo, live — and nothing else of yours.** The bottle checks out your history from a
  read-only mount of the repo's git objects, and fetches your upstream through your Mac. Your
  working copy, and the rest of your machine, are never mounted.
- **Git in, git out.** The repo is mounted read-only, and the bottle is served the git remotes
  it's given. One that takes pushes writes its own [namespace](https://git-scm.com/docs/gitnamespaces)
  and nothing else, can't touch your branches, and can't rewrite what you've already read.
- **Network on a leash.** No proxy to configure: every connection a bottle opens is made from
  your machine (a drone's, from its runner), decided by its machine's policy, so a bottle gets your
  VPN routes but not your local network, even as root.
- **Credentials that stay on your machine.** A feature can name the hosts its token belongs to, and
  folio attaches it there. The bottle never holds the token.
- **On another machine, too.** Add a Linux machine you reach over ssh as a *runner*, and a
  bottle there, a *drone*, works as one here does: its pushes, credentials and messages go
  through your Mac, which stays the one place your repo and tokens are.
- **Composable features.** `tools`, `jvm`, `python`, `cc`, `rust`, `chromium`, `claude`, `github`, `teamcity`, `develocity`, in the
  [Dev Container feature](https://containers.dev/implementors/features/) format, chosen in a spec
  file. Images are prebuilt, and rebuilt when they go stale.
- **No setup.** Installs what it needs on first use, and asks first. No Docker, no sudo, just
  Python and Homebrew.

## What a bottle is for

A bottle contains an agent's **mistakes**. A stray `rm -rf`, a bad merge, a build script that
rewrites a home directory, an agent that wanders out of its repo: none of it reaches your machine,
your other projects, or your branches. That's the boundary bottle is built to hold, and it holds
it without you watching.

A bottle is **not** a box for a hostile agent. Egress allows any public destination, so anything
inside a bottle — the repo, and anything you put there — can leave. Treat a bottle's contents as
the agent's to read and to send: don't put a secret in one you wouldn't hand to the agent
directly. Credentials folio manages stay out of the bottle: folio serve attaches them (see
[folio's Credentials](folio/README.md#credentials)), so they're never there to leave, though the
bottle can still spend them against the hosts they're attached to.

## Quick start

Bottle is the sandbox. To work in one -- a line of work with its own lane of branches, Claude
running in a session you can detach from, and pause and resume -- use [folio](folio/README.md), which makes
bottles for you:

```sh
cd ~/path/to/foo
folio configure --feature claude
folio claude fix-it       # A thread: a bottle, a lane for its commits, and Claude in it
```

`bottle` itself is bottle-cli, which runs every bottle's VM, its *machine*, here and on your
runners: `bottle ls` lists them (a thread's say which, as a label), and `bottle shell MACHINE`
opens a shell in one.

## Prerequisites

- macOS on Apple silicon
- Python 3.11+
- [Rust](https://rustup.rs), with the musl target for the VMs' agent
  (`rustup target add aarch64-unknown-linux-musl`), and Xcode's command line
  tools

Bottle builds bottle-cli, which runs its VMs, from this repo (`cargo xtask
dist`, into `target/dist`) when it's missing or its sources have changed;
`BOTTLE_CLI` names another to use instead.

A runner, for bottles on another machine, needs:

- Linux, with `/dev/kvm` usable by the user bottle logs in as (the `kvm` group)
- x86_64 or arm64
- git
- ssh from your Mac without a prompt: a key whose passphrase is in your
  Keychain (`ssh-add --apple-use-keychain`, `UseKeychain yes`), since bottle
  runs ssh in batch mode

Nothing else, and no root: bottle copies bottle-cli there, built on your Mac
for the runner's architecture: static, so it runs on any distribution. It's
all a runner has of bottle. Your Mac builds it in a Linux VM of its own bottle-cli
(`cargo xtask dist --target x86_64-unknown-linux-musl`, which bottle runs when
it's needed; the first time takes a while). (Untested on a real runner yet.)

## Commands

`bottle` is bottle-cli: machines, by their names, here and on every runner. Bottles -- a machine
with a repo, served its remotes, its features and credentials -- are [folio](folio/README.md)'s,
one for each thread (and its sidecar), as are credentials, features, and `folio serve`.

| Command | |
| --- | --- |
| `bottle ls` / `status MACHINE` | List machines, here and on every runner, and their state |
| `bottle shell MACHINE` | Open a shell in a machine, at its workdir |
| `bottle exec MACHINE` | Run one command in a machine |
| `bottle cp SRC DST` | Copy into, out of, or between machines |
| `bottle start` / `stop MACHINE` | Start or stop a machine |
| `bottle runner add` / `ls` / `check` / `detach` / `attach` / `rm` | The machines bottles run on: this one, and others over ssh |
| `bottle machine create` / `configure` / `delete` / `rename` / ... | A machine itself, as folio makes one |

<details>
<summary><b>Full reference</b> — every command, its arguments and its rules.</summary>

### `bottle runner add RUNNER --ssh DESTINATION` / `ls` / `check` / `detach` / `attach` / `rm`

A **runner** is a machine bottles run on. `local` is this Mac; another is a
Linux machine reached over ssh, and a bottle on one is a **drone**. Your Mac
stays the **controller**: your credentials and your repos are here, and what
a drone needs of them comes here. A repo's threads run on any runner
bottle-cli has (`folio configure --runner RUNNER`).

- **`bottle runner add RUNNER --ssh DESTINATION`** checks a machine and adds
  it under that name. `DESTINATION` is what you'd give ssh: a `Host` from
  `~/.ssh/config`, or `user@host`. It says what the machine lacks that bottle
  can't set up itself, and copies bottle-cli's distribution for the
  machine's architecture over, built here (see above): in a checkout,
  whenever bottle-cli's sources change, it's built (`bin/bottle-dist`) and
  copied again before the runner's next use.
- **`bottle runner ls`** lists them; **`bottle runner check RUNNER`** says
  what one is and lacks.
- **`bottle runner detach RUNNER [--offline]`** is for turning a runner off:
  it names the machines running there and asks before stopping them (in a
  terminal; told no, or with no one to ask, it stops nothing and leaves the
  runner attached, so you can have their agents wrap up first), and lets it
  go, then says it's safe to turn off. Until it's attached again, nothing
  asks it anything: `bottle ls` shows its machines as `detached`, at once.
  Using one of them again (a `bottle` command for it, or folio's `claude`,
  `shell`, and the like) attaches it again if it answers, and says it's still
  off if it doesn't. `--offline` marks one that's off already, unasked.
- **`bottle runner attach RUNNER`** attaches a detached runner again, once it
  answers, and names the machines detaching it stopped.
- **`bottle runner rm RUNNER`** forgets a runner with no machines. Nothing
  on the machine is touched.

On a runner, everything bottle keeps is in `~/.bottle`: bottle-cli's
distribution (`~/.bottle/dist`), the images built there, the machines, the
HTTP cache, and a bare mirror of each repo its bottles use (`~/.bottle/git`),
which your Mac pushes into as a drone is made, so its checkout borrows objects
from the runner's disk. Runners are bottle-cli's (`bottle-cli runner`): it
runs itself there over ssh, and a runner's machines are your Mac's by
`RUNNER/NAME` (`bottle shell desktop/foo`; `bottle ls` lists every
machine, here and on each runner); nothing else of bottle's runs there.

A drone works as a local bottle does, with these differences:

- **Its connections are decided on the runner**, by bottle-cli there and its
  machine's policy, so public destinations go straight from the runner, and
  "private" means the runner's network: a drone doesn't get your Mac's VPN
  routes. DNS is the runner's. The cached hosts are answered on the runner.
- **What its policy routes comes to your Mac**: its git (fetches served from
  your repo, its `origin` fetched first for `upstream`; pushes into it, under
  the same rules as a local bottle's), its credentialed requests, ssh and
  messages, to folio serve here, through sockets of its own on the runner
  (`~/.bottle/routes`) that ssh forwards back. The runner never holds a
  credential.
- **Those forwards are one ssh connection per drone**, kept by a process of
  bottle-cli's on your Mac while the drone runs, as a local machine has its
  own (its log: `~/.bottle/krun/runners/RUNNER/NAME/forward.log`). If it
  drops, the drone keeps its public network, while what's routed is refused
  until it's back: it connects again, waiting up to a minute between tries,
  and ends when the drone stops.

A runner that's off is unreachable, not broken: folio shows its
bottles as `unreachable`, a command for one fails at once, saying so, and
nothing else waits on it.

On a Mac, ssh's "No route to host" for a machine on your network is often
macOS refusing the local network to the process that asked: give the app you
run bottle from the Local Network permission (System Settings > Privacy &
Security > Local Network). folio serve has the permission of whatever started it.

### `bottle shell MACHINE`

Opens a login shell in the machine, at its workdir, as its user (a bottle's:
its first repo, as `genie`), starting it if it's stopped and it has its
network. `MACHINE` is its name as `bottle ls` has it: `RUNNER/NAME` for one on
a runner.

### `bottle exec MACHINE [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--] COMMAND [ARG]...`

Runs one command in the machine, as `shell` does, and exits with its status.
Put `--` before the command if it takes options of its own; `-t` gives it a
terminal.

```sh
bottle exec foo -- claude -p 'fix the failing test'
bottle exec foo -- cat /workspace/report.json | jq .failures
```

### `bottle cp SRC DST`

Copies SRC, a file or a directory, to DST, as `cp -r` would: into DST if it's
a directory, else as DST. `MACHINE:PATH` is a path in that machine (a relative
one is from its workdir), `RUNNER/NAME:PATH` one on a runner; any other is a
path here (`./a:b` is one). One side at least is in a machine: out of one, into
one, or between two, by way of this machine. A shorthand for `exec`, as
`shell` is: tar packs it on one side and unpacks it on the other, as the
machine's user there, and this machine's tar, which keeps it to where it's
unpacked, here. Either way it's unpacked beside DST first, so a copy that
fails leaves DST as it was.

```sh
bottle cp foo:build/reports/tests .
bottle cp ./fixtures foo:/workspace/src/test/
bottle cp foo:build/libs/app.jar desktop/foo-sidecar:
```

### `bottle start MACHINE` / `bottle stop MACHINE`

Starts or stops a machine's VM, without folio: what its policy routes to
folio serve is refused while it isn't running. Stopping keeps its disk.

### A machine's network

A machine decides each TCP connection the bottle opens itself, by the network
policy it was made with (`network.json` in its directory): public destinations
go straight there; private addresses are refused, but for a name allowed
private; and what the policy routes (folio's: `bottle.host`, ssh to other
bottles, and credentials' hosts) goes to the unix socket it names, after a
PROXY protocol header saying where it was going and from which machine. A
refused connection is reset, and logged in the machine's `boot.log`. A
bottle's names are looked up by the machine it runs on, with its resolver, as
they're asked; a bottle has no UDP or IPv6. A machine starts with nothing but
its name: `bottle start MACHINE` needs nothing else, and what it routes is
refused while nothing answers there.

For the hosts its policy routes by name and caches over https, a machine's
TLS is taken off with a CA of its own, made by `bottle-cli ca` in
`~/.bottle/krun/ca/<machine>` as it starts, and installed in that bottle's
trust store (whose bundle `NODE_EXTRA_CA_CERTS` names, for Node tools); a
machine with no such hosts has none. The CA carries a name constraint
permitting only those hosts, which the bottle's own TLS clients enforce: a
certificate for any other site is refused, whoever signs it. A constraint on a
host also covers the names below it (`github.com` covers `gist.github.com`);
that is how X.509 name constraints work. The keys never leave the machine the
bottle runs on, that machine never trusts any of them, and deleting the
machine removes them.

**The HTTP cache.** Every bottle starts from its image, so a build in a new
one downloads every dependency again. A feature can name hosts whose
downloads are kept: bottle-cli, which carries the bottle's connections,
terminates the bottle's TLS to them with the machine's CA (above), and answers a GET or HEAD from
`~/.bottle/cache/http/<host>/<path>` when it has the file, for every bottle.
Only whole 200s are kept, without headers (a hit has no `Content-Type`), unless
the server marks them `no-store`, `no-cache` or `private`, from GETs with no query, range,
`Authorization` or cookie, and bottle-cli fetches them with a request of its own,
following redirects, so a file is the same whoever asked first. Anything else
goes to the host as it was sent, but that a cached host's cookies are never
passed on: a bottle that sent one back would never be answered from the
cache. A kept file is never refreshed, so a feature
excludes the paths a host changes, such as an index of a package's versions; a request
with `Cache-Control: no-cache` fetches it again. A file no bottle has used for
a week is removed, when a bottle starts and every 6 hours while it runs; delete
the directory to empty it sooner. bottle-cli logs each request to a cached host
in the machine's `boot.log` (`~/.bottle/krun/machines/<machine>/boot.log`), and
whether it was a `hit`, `stored`, or `passed` on.

**apt's packages are kept too**, with nothing for a feature to name: bottle
knows Debian's repositories and those features declare, and keeps their
packages (each repository's `.deb` files, whose names carry their versions;
never its indexes), for every image built and every bottle, whatever the
server says about keeping them, since apt checks each against the signed
index. Debian's are asked over plain http, which bottle-cli answers without
terminating anything and fetches over https. An https repository at build
time is answered behind a caching CA builds share (`bottle-builds`), which the
build's apt step trusts for those hosts alone; a bottle trusts its machine's
CA for them, as for any cached host.

</details>

## Development

Run the tests from the repo root:

```sh
python3 -m unittest
```

On a Mac with bottle-cli built (`cargo xtask dist`, or `BOTTLE_CLI` naming
one), that includes two suites that run real machines, and take minutes the
first time, while they pull and build images:

- `tests/test_krun_contract.py`: what bottle relies on bottle-cli for. Run it
  after changing bottle-cli, the agent or libkrun.
- `tests/test_end_to_end.py`: a real bottle, made as folio makes one and run in through `bin/bottle`, in its own
  `BOTTLE_HOME` (`BOTTLE_E2E_HOME`, default `/tmp/bottle-e2e`).

Elsewhere they're skipped. Runners are tested here with a fake ssh, which
runs each command on this machine, in a home of its own: `tests/test_drone.py`
is a drone end to end, with only ssh and bottle-cli faked.

bottle-cli's own tests are Rust's: `cargo test`. Those that need root (they
stack disks with loop devices, in namespaces) skip without it; to run them,
as root but built as you:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test
```
