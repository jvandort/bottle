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

`bottle` itself is bottle-cli, which runs every bottle's VM, its *machine*: `bottle ls` lists
this machine's (a thread's say which, as a label), and `bottle shell MACHINE` opens a shell in
one. A runner's are its own bottle-cli's, which folio runs there.

## Prerequisites

- macOS on Apple silicon
- Python 3.11+
- [Rust](https://rustup.rs), with the musl target for the VMs' agent
  (`rustup target add aarch64-unknown-linux-musl`), and Xcode's command line
  tools

Bottle builds bottle-cli, which runs its VMs, from this repo (`cargo xtask
dist`, into `target/dist`) when it's missing or its sources have changed;
`BOTTLE_CLI` names another to use instead.

bottle-cli is built for Linux too, x86_64 and arm64, static, so it runs on any
distribution: what [folio](folio/README.md#runners) copies to a runner, the
other machines it makes bottles on, where it's all there is of bottle. Your
Mac builds Linux's in a Linux VM of its own bottle-cli, whenever it builds its
own (in a checkout, `bin/bottle` does; the first time takes a while).

## Commands

`bottle` is bottle-cli: this machine's machines, by their names. Bottles -- a machine with a
repo, served its remotes, its features and credentials -- are [folio](folio/README.md)'s, one for
each thread (and its sidecar), as are credentials, features, `folio serve`, and runners: other
machines, where folio runs their bottle-cli over ssh.

| Command | |
| --- | --- |
| `bottle ls` / `status MACHINE` | List machines, and their state |
| `bottle shell MACHINE` | Open a shell in a machine, at its workdir |
| `bottle exec MACHINE` | Run one command in a machine |
| `bottle cp SRC DST` | Copy into, out of, or between machines |
| `bottle start` / `stop MACHINE` | Start or stop a machine |
| `bottle create` / `configure` / `rename` / `delete MACHINE` | A machine itself, as folio makes one |

<details>
<summary><b>Full reference</b> — every command, its arguments and its rules.</summary>

### `bottle shell MACHINE`

Opens a login shell in the machine, at its workdir, as its user (a bottle's:
its first repo, as `genie`), starting it if it's stopped and it has its
network. `MACHINE` is its name as `bottle ls` has it.

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
one is from its workdir); any other is a path here (`./a:b` is one). One side at least is in a machine: out of one, into
one, or between two, by way of this machine. A shorthand for `exec`, as
`shell` is: tar packs it on one side and unpacks it on the other, as the
machine's user there, and this machine's tar, which keeps it to where it's
unpacked, here. Either way it's unpacked beside DST first, so a copy that
fails leaves DST as it was.

```sh
bottle cp foo:build/reports/tests .
bottle cp ./fixtures foo:/workspace/src/test/
bottle cp foo:build/libs/app.jar bar:
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
TLS is taken off with a CA of its own, made in `~/.bottle/krun/ca/<machine>`
as it starts. Its certificate is given to the bottle at `/run/bottle/ca.crt`,
on `/run`'s tmpfs, as it boots, for the image to trust: bottle's base image
puts it in its trust store as it starts (whose bundle `NODE_EXTRA_CA_CERTS`
names, for Node tools). A machine with no such hosts has none. The CA carries a name constraint
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

bottle-cli's tests are Rust's: `cargo test`. Those that need root (they
stack disks with loop devices, in namespaces) skip without it; to run them,
as root but built as you:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test
```

`bin/bottle` and `bin/folio`, shell scripts, build bottle-cli from the
checkout first when its sources have changed since it was (`bin/bottle-build`:
a hash of their contents, `sha256sum`'s, against the one the last build left,
then one `cargo xtask dist` for every distribution whose sources differ,
which builds and copies only what's changed: this machine's and the ones
runners take, on a Mac Linux's two, by one cargo in a Linux VM, and on Linux
the other architecture's, by the same cargo as its own, with Debian's cross
gcc), then run bottle-cli, or folio with `BOTTLE_CLI` naming it. `bin/folio`
builds folio-cli too, the same way (`bin/folio-build`, its own sources' hash),
and sets `FOLIO_CLI`. Nothing else builds them: folio runs `$BOTTLE_CLI`, else
`bottle-cli` on `PATH`, and `$FOLIO_CLI`, else `folio-cli` on `PATH`. folio's tests,
the suites that drive bottle-cli from folio's side among them, are in
[folio](folio/README.md#development).
