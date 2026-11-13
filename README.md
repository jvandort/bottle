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
  your machine (a drone's, from its runner), after `folio serve` decides it, so a bottle gets your
  VPN routes but not your local network, even as root.
- **Credentials that stay on your machine.** A feature can name the hosts its token belongs to, and
  `folio serve` attaches it there. The bottle never holds the token.
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
directly. Credentials bottle itself manages stay out of the bottle: `folio serve` attaches them
(see `folio auth login`), so they're never there to leave, though the bottle can still spend them
against the hosts they're attached to.

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
runners: `bottle ls` lists them, and `bottle shell MACHINE` opens a shell in one (`folio bottle ls`
says which is whose).

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

Bottles are made, kept and deleted by [folio](folio/README.md), one for each thread (and its
sidecar). What's left is a machine's, by its name, and bottle's parts:

| Command | |
| --- | --- |
| `bottle ls` / `status MACHINE` | List machines, here and on every runner, and their state |
| `bottle shell MACHINE` | Open a shell in a machine, at its workdir |
| `bottle exec MACHINE` | Run one command in a machine |
| `bottle cp SRC DST` | Copy into, out of, or between machines |
| `bottle start` / `stop MACHINE` | Start or stop a machine |
| `folio auth login` / `list` / `set` / `logout` | Store a credential once, for every bottle |
| `folio runner add` / `ls` / `stop` / `detach` / `attach` / `rm` | The machines bottles run on: this one, and others over ssh |
| `folio bottle ls` / `rm` | Every bottle, whose it is, and its machine; delete one no thread has |
| `folio serve start` / `stop` | `folio serve`, which answers what bottles' machines route to it |
| `folio shutdown` | Stop every running bottle, then `folio serve` |

<details>
<summary><b>Full reference</b> — every command, its arguments and its rules.</summary>

### `folio auth login CREDENTIAL`

Logs in once, for every bottle. Features declare the credentials they need (the
`claude` feature needs `claude`, a Claude subscription token; the `github`
feature needs `github`, a personal access token; the `teamcity` feature needs
`teamcity`, an access token; the `develocity` feature needs `develocity`, an
access key, which it reads from Gradle's own key file when that has one). `login` runs the feature's own login
command (`claude setup-token`) in a throwaway bottle, attached to your terminal,
captures the credential it prints and stores it in the macOS Keychain; a
credential with no such command is asked for instead.

A credential never enters a bottle. folio serve keeps it, and attaches it as a header to the bottle's requests to the hosts the
feature named — `claude` names `api.anthropic.com`, `github` names `github.com`
and its API, `teamcity` and `develocity` name their `server`. The agent can spend the credential
against those hosts and can't read it, so an agent that sends everything it
holds to a stranger sends no token.

Spending it is not a free-for-all, though. Some requests would turn the
credential into one the bottle could hold: a token minted from it, an access
key provisioned, an SSH key or a deploy key added, a runner registered, a
codespace started. Each feature lists those routes (`forbid`), and folio serve
refuses them outright, with the credential or without. Beyond those, each
attaches its credential only to the routes its tools were seen to use
(`allow`): `github` to reads (gh's REST reads and GraphQL, clones and
fetches), so no push and no REST write; `claude` to Claude Code's own
requests. A refused request comes back 403, saying why and that only the user
can allow it, as JSON to a client that asked for JSON, so gh and Claude Code
show it. A feature's `forbid` is also an assertion over its `allow`: an
allowed route that could reach a forbidden one fails the feature's loading,
as allowed too widely. GraphQL is one route, `POST /graphql`, so a mutation through it isn't
refused, so still give each credential no more than a bottle should have: a
fine-grained GitHub token without Workflows or Administration (or read-only),
a Develocity user without administrative permissions.

Tools in the bottle talk to those hosts over HTTPS as usual. TLS is opaque, so
for those hosts only, the bottle's machine routes the connection to folio serve:
bottle-cli, which carries the machine's connections, completes the bottle's TLS
handshake with a certificate from the machine's **CA**, and gives folio serve the
plain request, after a PROXY protocol header saying where it was going and
from which machine. folio serve attaches the credential and makes its own verified
HTTPS connection to the server. Every other connection goes straight to where
it was headed, untouched. The host is also reachable when it's a private
address, since configuring it is what naming it means; nothing else about that
host opens up.

Each machine has a CA of its own, made by `bottle-cli ca` in
`~/.bottle/krun/ca/<machine>` as it starts, and installed in that bottle's
trust store (whose bundle `NODE_EXTRA_CA_CERTS` names, for Node tools); a bottle whose
features name no credential or cached hosts has none. The CA carries a name constraint
permitting only the hosts the machine's policy routes by name and caches over https, which
the bottle's own TLS clients enforce: a certificate for any other site is refused, whoever
signs it, so nothing but those hosts' traffic can be read. A constraint
on a host also covers the names below it (`github.com` covers
`gist.github.com`); that is how X.509 name constraints work. The keys never
leave the machine the bottle runs on, that machine never trusts any of them, and
deleting the machine removes them.

A CLI doesn't know its token is being attached for it, and usually won't make a
request until it thinks it's logged in, so a bottle gets a **stand-in**: a fake
token, never a secret. A feature either writes one itself (`teamcity` puts one
in the CLI's config file) or names an environment variable for bottle to set in
every bottle with that feature (`claude` names `CLAUDE_CODE_OAUTH_TOKEN`). The
throwaway bottle a login command runs in gets none, so `folio auth login
claude` starts from nothing rather than from a token that isn't one.

Folio logs in for you when a bottle's features need a credential that isn't
set yet, as it makes or starts the bottle.

```sh
folio auth login claude
folio auth login teamcity
```

### `folio auth list` / `folio auth set CREDENTIAL` / `folio auth logout CREDENTIAL`

Lists declared credentials and which are set; stores one read from stdin (for
scripts); removes one. Bottles pick up changes when they next start.

### Features

A bottle's image is built with its features when it's made, if it isn't built
yet. Each image records what it was built from, so an image whose features (or
the base underneath) have changed since is stale: the next bottle rebuilds it,
and after any build bottle deletes its stale images that no bottle uses. Each
runner keeps its own images, built for its own CPU.

Features, in the [Dev Container feature](https://containers.dev/implementors/features/)
format (a `devcontainer-feature.json` and an `install.sh` per directory):

- `tools`: a minimal working environment (ripgrep, fd, jq, curl, vim, tmux, shpool, ...).
- `jvm`: Eclipse Temurin JDKs, from Adoptium's signed apt repository. Options:
  `version` (default `25`), the default `java` and `JAVA_HOME`; and
  `additionalVersions`, more JDKs alongside it, e.g. `17,21`. Every JDK is in
  `/usr/lib/jvm` (also as `/usr/lib/jvm/jdk-<version>`), where tools like
  Gradle's toolchains find them.
- `python`: Debian's Python 3, with `venv`, `pip`, and `python` as a name for
  it. `pip install` works without a virtualenv: Debian's PEP 668 marker is
  removed, since a bottle is disposable and a new one undoes whatever an
  install breaks. Option: `externallyManaged` (default `false`) keeps the
  marker, and Debian's behaviour, if you'd rather work in a venv.
- `cc`: gcc as `cc`, with the C library headers (`libc6-dev`, which gcc only
  recommends and every link needs): what cargo needs to link build scripts,
  even when the output is wasm. No g++ or make.
- `rust`: Rust through rustup 1.29.2 (checked against its checksum): a
  toolchain with clippy and rustfmt, in a shared `RUSTUP_HOME`
  (`/usr/local/rustup`) the bottle's user owns, so `rustup target add` needs no
  sudo; `cargo`, `rustc` and `rustup` are in `/usr/local/bin`. `cargo install`
  puts programs in `~/.cargo/bin`, on login shells' PATH. Brings `cc`.
  Options: `toolchain` (default `stable`), `components` (default
  `clippy,rustfmt`) and `targets` (default none), comma-separated.
- `chromium`: Playwright for Python, with the headless Chromium build it's
  made for (`playwright install --with-deps --only-shell chromium`, at
  `/opt/ms-playwright`, in the image), to see a built web page actually
  render; Chromium's own `--screenshot` captures before scripts have run.
  Brings `python`. Option: `playwright` (default `1.64.1`); its Chromium comes
  with it.
- `claude`: Claude Code, from Anthropic's signed apt repository, ready to work:
  every directory is trusted (the bottle is the sandbox), first-run setup is done, it reaches the
  API through the egress proxy, which holds the `claude` credential
  (`folio auth login claude`) so the agent can spend its own subscription
  token without ever being able to read it, and it reads
  bottle's context for the agent (`~/BOTTLE.md`) as its instructions. Sessions
  start on Opus 5.5 at medium effort. It also
  has a channel, `bottle-event`, an MCP server that brings events into a
  running session (`bottle exec MACHINE -- bottle-event send -f FROM TEXT`, which
  waits until the session takes it) and gives the agent a `send` tool for
  messages to other bottles: folio serve delivers each one as an event whose `from`
  is an address for answering the sender, issued to the recipient and good
  from no other bottle, and answers once that session has taken it (404 for no
  running bottle the sender may reach that way, 504 for no session that took it
  in time). Only a bottle with the `message-any` permission (see its spec, below)
  can start a conversation, by a bottle's name; any other can only answer.
  Claude Code
  runs the channel when started with `--dangerously-load-development-channels
  server:events`, as folio starts it. Options:
  `permissionMode` (default `bypassPermissions`: the bottle is the sandbox),
  `theme` (default `dark`), and `tui` (default `default`: `fullscreen` would
  capture the mouse, and a bottle has no clipboard to copy to instead).
- `github`: GitHub's CLI, `gh`, from GitHub's signed apt repository, with `gh`
  and git both authenticated to github.com by the egress proxy, which holds the
  `github` credential. `folio auth login github` asks for a
  [fine-grained personal access token](https://github.com/settings/personal-access-tokens):
  what the token may do is what a bottle may do on GitHub, and a push there
  doesn't pass through your repo the way a push to a bottle's remote does, so give
  it just the repositories and permissions the agent needs. GitHub's ssh
  remotes (`git@github.com:...`) are fetched over https instead, since a bottle
  has no ssh key.

  ```sh
  folio configure --feature github
  ```
- `teamcity`: JetBrains' [TeamCity CLI](https://www.jetbrains.com/help/teamcity/teamcity-cli.html),
  verified against JetBrains' signature from its GitHub release, with its access token
  held by the egress proxy rather than the bottle (see `folio auth login`).
  Options: `server` (required), the TeamCity server, e.g.
  `https://teamcity.example.com`; `version`, the CLI release (default `latest`); and `readOnly`
  (default `true`), which blocks everything but GET, so a bottle can read
  builds but not start or change anything; pass `readOnly=false` to let it
  start them. A token with read-only permissions is the real boundary.

  ```sh
  folio configure --feature teamcity:server=https://teamcity.example.com
  ```
- `develocity`: [Develocity's MCP server](https://docs.develocity.ai/2026.3/integrations/agentic-ai/mcp-servers/ai-agents/)
  for Claude Code, with its access key held by the egress proxy rather than
  the bottle. The key is read on the machine from Gradle's
  `~/.gradle/develocity/keys.properties` (`server=key` lines, as
  `./gradlew provisionDevelocityAccessKey` writes them) each time the bottle
  starts, so there's nothing to log in to; a server the file has no line for
  uses `folio auth login develocity` instead. Develocity serves MCP itself, so
  nothing is installed: the server goes in Claude Code's user config, which
  the feature writes whether or not `claude` is in the spec (it installs after
  it when it is). The server's API is authenticated the same way. Options:
  `server` (required), the Develocity servers, separated by commas, e.g.
  `https://develocity.example.com`, each with its own key and its own MCP
  server in Claude Code, `develocity-<host>` (`develocity-develocity-example-com`); `keyFile`, where the keys are (default
  `~/.gradle/develocity/keys.properties`). The key's user needs the "Access build
  data via the API" permission, and should have no administrative ones. The
  bottle's own `~/.gradle/develocity/keys.properties` has a line for each
  server with the placeholder in place of its key, so Gradle needs nothing set:
  folio serve puts the key where the Develocity plugin presents it, for Build Scans,
  the build cache, test distribution and predictive test selection, and refuses
  to mint tokens (`/api/auth/token`) or provision access keys. A server on a private address, as one behind a
  VPN often is, is reachable because the credential names it.

  ```sh
  folio configure --feature develocity:server=https://develocity.example.com,https://ge.example.com
  ```

A feature can take options: `FEATURE:OPTION=VALUE[,OPTION=VALUE]`, e.g.
`--feature jvm:version=21,additionalVersions=17,11`. Options left out take
their defaults.

bottle runs features itself (no Dev Container tooling or Node) and supports a
subset of the format: `id`, `version`, metadata, `options` (string and boolean),
`containerEnv`, `dependsOn` / `installsAfter` naming local features, and
`customizations.bottle` (the credentials a feature needs, the hosts each one is
attached to, the stand-in it wants and the routes it's refused, allowed or
rewritten on, `cache`, the hosts whose downloads are
kept, `apt`, the repositories and packages it installs, all features' together
in one step before their `install.sh`, `requiredOptions`, the options a user
has to set, and `context`, a sentence or two for the agent about what the
feature gives it, added to `~/BOTTLE.md` with `${option}` filled in).
Anything else in a definition is an error. Remote features aren't supported.

### A bottle: its spec, repos, remotes and ssh

Folio makes each bottle (bottle/bottles.py, in folio's process) from a
**spec**: its features and memory (`folio configure` sets a repo's). Its name
is its identity, and its thread's: a name that's taken is refused. Its
record is kept with its thread, in folio's home
(`~/.folio/<repo>/<thread>/bottles/<name>.json`); one no thread has is in
`~/.folio/bottles`, and `folio bottle rm` deletes it.

```json
{"features": ["claude", "jvm:version=21", "tools"], "memory": "8G"}
```

A spec can give the bottle's `disk`, in G (`"disk": "50G"`): the disk what
it writes goes to, sparse, so it costs what it holds; 64G if it doesn't say.
Its VM gets every core of the machine it runs on, up to 16.

A spec can also give the bottle `permissions`, what folio serve lets it do beyond
what every bottle may: `message-any`, to send messages to a running bottle by
its name rather than only answer one; and `list-bottles`, to list the other
running bottles, each one's name, repo and `labels` (which a spec can give,
`{"labels": {"thread": "fix-it"}}`; folio's say which thread a bottle is for).
folio serve checks the bottle's spec, never the bottle; the bottle sees its
permissions as `BOTTLE_PERMISSIONS`, so its tools know what to offer. Folio
never gives a thread's bottle permissions.

Each of its repos is checked out at a path of its own, any absolute path
(folio's: the thread's repo at `/workspace`, its notes at `/notes`). Its objects are mounted read-only, so nothing is
cloned and the bottle can't change the repo except by pushing to a remote
it's served. The first is its machine's workdir, where `bottle shell` and
`bottle exec` start, and where an SSH login goes. What's checked out there is
a branch (the repo's, else origin's) or a commit, by default the origin
remote's default branch, or, if there's no origin, whatever the repo has
checked out.

Each is served the remotes it's given, `NAME:fetch=VIEW[,push=ns/NAMESPACE]`, live, at
`http://bottle.host/git/<PATH>/<NAME>`, which folio serve answers. What a fetch
sees is one of three views:

- `upstream`: your repo's upstream (its `origin/*` branches), read-only.
  Fetching first fetches your repo's `origin` on your Mac, so the bottle gets
  the upstream's latest even if you haven't fetched.
- `heads`: your repo's own branches: how the agent sees what you've integrated.
- `ns/NAMESPACE`: the refs in one [git namespace](https://git-scm.com/docs/gitnamespaces)
  of your repo, and nothing else.

With `push`, pushes go into that namespace. **Your checked-out branches follow
a push**: a branch of yours whose upstream is a remote reading that namespace
(folio's `bottle/<thread>`, so any branch you got with `git switch` after
fetching it) is fast-forwarded to what the bottle pushed, files and all, in
whichever worktree has it checked out, and the remote's tracking refs are
fetched. Only when nothing would be lost: a branch with commits of yours, a
change in the way, or a worktree in the middle of a merge, rebase,
cherry-pick, revert or bisect is left alone; so is a branch checked out
nowhere, though one you're reading in curate comes along by curate's own hook:
the fetch runs with your repo's hooks, as yours would. The push succeeds either way, and its
output tells the bottle what moved and what didn't (`remote: bottle: ...`);
folio serve logs it too. `git config bottle.follow false` in your repo turns it
off. A namespace is what confines the
bottle: every ref its `receive-pack` resolves is inside it, and your own refs
aren't advertised to it at all. Pushes are append-only, refusing force-pushes
and deletes, so nothing you've already read changes underneath you; no other
bottle may push into the same namespace, but one it may ssh to; and your repo's own hooks don't run.
Reading one namespace and writing another makes a two-lane exchange, where
neither side's push is ever refused for what the other did: that's how
[folio](folio/README.md) gives each thread its lane (`origin:fetch=upstream`,
`host:fetch=heads`, `agent:fetch=ns/T-out,push=ns/T-out`, `user:fetch=ns/T-in`).
A repo given none gets `origin` (`upstream`) and `host` (`heads`), both read-only.

`~/BOTTLE.md` says what the bottle, its remotes and its features are, and,
after that, what its maker adds: folio's say how to work in a thread.

Its machine runs on a runner: `local`, this machine, by default, or one added
with `folio runner add`. The image is built there if it isn't yet, and the
spec's memory must fit that machine.

A bottle can be let ssh to another (folio's thread, to its sidecar), which must
exist already: `ssh NAME` in this one is a login there, as `genie`, and so are scp and git over ssh. That's all it
reaches: NAME's address goes to that bottle's sshd and nowhere else, never to
a machine on your network. It's the bottle as it was when the grant was made:
one made again under the same name isn't reached until this one is made again
too. Bottle doesn't start it: start both. The two may push into the same
namespaces, as two people share a branch: a push that lost a race is refused,
not lost.

bottle-cli keeps its state (machines, images, the HTTP cache, runners) in
`~/.bottle`, and folio its own in `~/.folio`; `BOTTLE_HOME` and `FOLIO_HOME`
name others.

### `folio runner add RUNNER --ssh DESTINATION` / `ls` / `stop` / `detach` / `attach` / `rm`

A **runner** is a machine bottles run on. `local` is this Mac; another is a
Linux machine reached over ssh, and a bottle on one is a **drone**. Your Mac
stays the **controller**: the registry, your credentials and your repos are
here, and what a drone needs of them comes here.

- **`folio runner add RUNNER --ssh DESTINATION`** checks a machine and adds
  it under that name. `DESTINATION` is what you'd give ssh: a `Host` from
  `~/.ssh/config`, or `user@host`. It says what the machine lacks that bottle
  can't set up itself, and copies bottle-cli's distribution for the
  machine's architecture over, built here (see above).
  Whenever bottle-cli's sources change, it's built and copied again before
  the runner's next bottle is made.
- **`folio runner ls [RUNNER]...`** (also `list`, `status`) shows each runner, whether it can be
  reached, what it lacks, and its bottles.
- **`folio runner stop RUNNER`** stops every bottle on a runner, so the
  machine can be shut down. Their disks stay there; each starts again when it's next used.
- **`folio runner detach RUNNER [--offline]`** is for turning a runner off:
  it names the bottles running there and asks before stopping them (in a
  terminal; told no, or with no one to ask, it stops nothing and leaves the
  runner attached, so you can have their agents wrap up first), checks that nothing else runs there (an image build,
  say; if something does, it says what, and leaves the runner attached), and
  lets it go, then says it's safe to turn off. Until it's attached again,
  nothing asks it anything: its bottles show as `detached`, at once. Using one of its bottles again
  (folio's `claude`, `shell`, and the like) attaches it again if
  it answers, and says it's still off if it doesn't. `--offline` marks one
  that's off already, unasked.
- **`folio runner attach RUNNER`** attaches a detached runner again, once it
  answers, and names the bottles detaching it stopped.
- **`folio runner rm RUNNER`** forgets a runner with no bottles. Nothing
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

A runner that's off is unreachable, not broken: `folio bottle ls` shows its
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

### `folio bottle ls [--json]` / `folio bottle rm BOTTLE`

Every bottle, its thread (`REPO/THREAD`), its state, its runner and its
machine; a bottle on a runner that can't be reached shows as `unreachable`.
`rm` deletes one no thread has (a thread's goes with it: `folio delete`, or
`folio pause`), and everything it created; if making or deleting one was
interrupted, `rm` cleans up whatever is left. Whatever it pushed is kept, in
your repo's namespaces, and its image, so the next bottle with the same
features starts without a build.

### `folio shutdown`

Stops every running bottle, then `folio serve`. Bottles on a runner that can't be
reached are left as they are, and it says so.

### `folio serve start` / `folio serve stop`

Starts or stops `folio serve`, the background process that serves what bottles'
machines route to it. A machine decides each TCP connection the bottle opens
itself, by the network policy it was made with (`network.json` in its
directory): public destinations go straight there; private addresses are
refused, but for a name allowed private; `bottle.host`, ssh to other bottles
and credentials' hosts are routed to folio serve's sockets, one per service (`~/.folio/services`),
which answers `bottle.host` with the bottle's git remotes. A refused
connection is reset, and logged in the machine's `boot.log`. A bottle's names
are looked up by the machine it runs on, with its resolver, as they're asked;
a bottle has no UDP or IPv6. A machine starts with nothing but its name
(`bottle start MACHINE` works without folio serve); what it routes to folio serve is
refused while folio serve isn't running. folio serve logs one line per connection it
serves, in `~/.folio/logs/serve.log`. Its code is still bottle's (bottle/daemon.py).

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

folio serve starts automatically when a command needs it, and when it starts it
picks up the bottles there are. While it's stopped, a running bottle's
connections go on as its policy says, but for what it routes to folio serve
(`bottle.host`, ssh, credentials' hosts), which is refused. A command that finds it running other code (another
checkout's, or this one's before it changed) restarts it first, and says so.

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
