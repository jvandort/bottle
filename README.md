# Bottle

*The perfect home for a genie.*

Bottle manages sandboxed Linux VMs for coding agents, on macOS. Let the agent run with full
permissions inside. Take back only the commits you want.

- **A real VM per bottle.** Its own kernel, via [apple/container](https://github.com/apple/container),
  with everything it needs already installed. Starts in about a second.
- **Your repo, live — and nothing else of yours.** The bottle checks out your history from a
  read-only mount of the repo's git objects, and fetches your upstream through the host. Your
  working copy, and the rest of your machine, are never mounted.
- **Git in, git out.** The repo is mounted read-only, and the bottle is served the git remotes
  it's given. One that takes pushes writes its own [namespace](https://git-scm.com/docs/gitnamespaces)
  and nothing else, can't touch your branches, and can't rewrite what you've already read.
- **Network on a leash.** Egress only through a proxy on your machine, so a bottle gets your DNS and
  VPN routes but not your local network, even as root.
- **Credentials that stay on your machine.** A feature can name the hosts its token belongs to, and
  the proxy attaches it there. The bottle talks to the proxy and never holds the token.
- **Composable features.** `tools`, `jvm`, `python`, `claude`, `github`, `teamcity`, in the
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
directly. Credentials bottle itself manages stay out of the bottle: the egress proxy attaches them
(see `bottle auth login`), so they're never there to leave, though the bottle can still spend them
against the hosts they're attached to.

## Quick start

Bottle is the sandbox. To work in one -- a line of work with its own lane of branches, Claude
running in tmux, and pause and resume -- use [folio](folio/README.md), which drives bottle for
you:

```sh
cd ~/path/to/foo
folio configure set --feature tools --feature claude
folio claude fix-it       # A thread: a bottle, a lane for its commits, and Claude in it
```

Bottle on its own:

```sh
echo '{"features": ["tools", "claude"]}' > spec.json
bottle new --spec spec.json --name foo --git-repo ~/path/to/foo
bottle shell foo          # Open a shell at /workspace; run `claude` here
```

## Prerequisites

- macOS 26+ on Apple silicon
- [Homebrew](https://brew.sh)
- Python 3.11+

Bottle sets up what it depends on (apple/container, its services
and a Linux kernel) as necessary, and asks before installing anything.

## Commands

| Command | |
| --- | --- |
| `bottle auth login` / `list` / `set` / `logout` | Store a credential once, for every bottle |
| `bottle new` | Create a bottle from a spec |
| `bottle spec validate` | Check a spec, and print it in canonical form |
| `bottle shell` | Open a shell in a bottle, at `/workspace` |
| `bottle exec` | Run one command in a bottle |
| `bottle status` | List bottles and their state |
| `bottle start` / `stop` / `delete` | Control a bottle's lifecycle |
| `bottle build` | Build an image with the given features |
| `bottle daemon start` / `stop` | Manage `bottled`, the background network process |
| `bottle shutdown` | Stop every running bottle and the anchor, then `bottled` |
| `bottle egress` | Run a bottle's egress proxy by hand, for testing |

<details>
<summary><b>Full reference</b> — every command, its arguments and its rules.</summary>

### `bottle auth login CREDENTIAL`

Logs in once, for every bottle. Features declare the credentials they need (the
`claude` feature needs `claude`, a Claude subscription token; the `github`
feature needs `github`, a personal access token; the `teamcity` feature needs
`teamcity`, an access token). `login` runs the feature's own login
command (`claude setup-token`) in a throwaway bottle, attached to your terminal,
captures the credential it prints and stores it in the macOS Keychain; a
credential with no such command is asked for instead.

A credential never enters a bottle. bottled hands it to that bottle's egress
proxy, which attaches it as a header to the bottle's requests to the hosts the
feature named — `claude` names `api.anthropic.com`, `github` names `github.com`
and its API, `teamcity` names its `server`. The agent can spend the credential
against those hosts and can't read it, so an agent that sends everything it
holds to a stranger sends no token.

Tools in the bottle talk to those hosts over HTTPS as usual. A CONNECT tunnel is
opaque, so for those hosts only, the proxy terminates it: it completes the
bottle's TLS handshake with a certificate from the bottle's **egress CA**,
reads the request, attaches the credential, and makes its own verified HTTPS
connection to the server. Every other destination is tunnelled untouched. The
host is also reachable when it's a private address, since configuring it is
what naming it means; nothing else about that host opens up.

Each bottle has an egress CA of its own, made with `openssl` in
`~/.bottle/ca/<bottle>` when the bottle starts, and installed in that bottle's
trust store (and at `NODE_EXTRA_CA_CERTS`, for Node tools); a bottle whose
features name no credential hosts has none. The CA carries a name constraint
permitting only the hosts the bottle's credentials name, which the bottle's own
TLS clients enforce: a certificate for any other site is refused, whoever signs
it, so nothing but those hosts' traffic can be read by the proxy. A constraint
on a host also covers the names below it (`github.com` covers
`gist.github.com`); that is how X.509 name constraints work. The keys never
leave your machine, your machine never trusts any of them, and `bottle delete`
removes them.

A CLI doesn't know its token is being attached for it, and usually won't make a
request until it thinks it's logged in, so a bottle gets a **stand-in**: a fake
token, never a secret. A feature either writes one itself (`teamcity` puts one
in the CLI's config file) or names an environment variable for bottle to set in
every bottle with that feature (`claude` names `CLAUDE_CODE_OAUTH_TOKEN`). The
throwaway bottle a login command runs in gets none, so `bottle auth login
claude` starts from nothing rather than from a token that isn't one.

`bottle new`, `shell` and `start` log in for you when a bottle's
features need a credential that isn't set yet.

```sh
bottle auth login claude
bottle auth login teamcity
```

### `bottle auth list` / `bottle auth set CREDENTIAL` / `bottle auth logout CREDENTIAL`

Lists declared credentials and which are set; stores one read from stdin (for
scripts); removes one. Bottles pick up changes when they next start.

### `bottle build [--feature FEATURE]... [--no-cache]`

Builds the bottle image with the given features installed; `bottle new` does
this itself when needed. Each image records what it was built from, so an image
whose features (or the base underneath) have changed since is stale: `bottle
new` rebuilds it, and after any build bottle deletes its stale images that no
bottle uses.

```sh
bottle build --feature tools --feature claude
```

Features, in the [Dev Container feature](https://containers.dev/implementors/features/)
format (a `devcontainer-feature.json` and an `install.sh` per directory):

- `tools`: a minimal working environment (ripgrep, fd, jq, curl, vim, tmux, ...).
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
- `claude`: Claude Code, from Anthropic's signed apt repository, ready to work:
  the bottle's `/workspace` is trusted, first-run setup is done, it reaches the
  API through the egress proxy, which holds the `claude` credential
  (`bottle auth login claude`) so the agent can spend its own subscription
  token without ever being able to read it, and it reads
  bottle's context for the agent (`~/BOTTLE.md`) as its instructions. Options:
  `permissionMode` (default `bypassPermissions`: the bottle is the sandbox),
  `theme` (default `dark`), and `tui` (default `default`: `fullscreen` would
  capture the mouse, and a bottle has no clipboard to copy to instead).
- `github`: GitHub's CLI, `gh`, from GitHub's signed apt repository, with `gh`
  and git both authenticated to github.com by the egress proxy, which holds the
  `github` credential. `bottle auth login github` asks for a
  [fine-grained personal access token](https://github.com/settings/personal-access-tokens):
  what the token may do is what a bottle may do on GitHub, and a push there
  doesn't pass through your repo the way a push to a bottle's remote does, so give
  it just the repositories and permissions the agent needs. GitHub's ssh
  remotes (`git@github.com:...`) are fetched over https instead, since a bottle
  has no ssh key.

  ```sh
  echo '{"features": ["tools", "github"]}' > spec.json
  bottle new --spec spec.json --name reponame --git-repo ~/src/reponame
  bottle exec reponame -- gh pr list
  ```
- `teamcity`: JetBrains' [TeamCity CLI](https://www.jetbrains.com/help/teamcity/teamcity-cli.html),
  pinned and checksum-verified from its GitHub release, with its access token
  held by the egress proxy rather than the bottle (see `bottle auth login`).
  Options: `server` (required), the TeamCity server, e.g.
  `https://teamcity.example.com`; `version`, the CLI release; and `readOnly`
  (default `true`), which blocks everything but GET, so a bottle can read
  builds but not start or change anything; pass `readOnly=false` to let it
  start them. A token with read-only permissions is the real boundary.

  ```sh
  echo '{"features": ["tools", "teamcity:server=https://teamcity.example.com"]}' > spec.json
  bottle new --spec spec.json --name reponame --git-repo ~/src/reponame
  bottle exec reponame -- teamcity run list
  ```

A feature can take options: `FEATURE:OPTION=VALUE[,OPTION=VALUE]`, e.g.
`--feature jvm:version=21,additionalVersions=17,11`. Options left out take
their defaults.

bottle runs features itself (no Dev Container tooling or Node) and supports a
subset of the format: `id`, `version`, metadata, `options` (string and boolean),
`containerEnv`, `dependsOn` / `installsAfter` naming local features, and
`customizations.bottle` (the credentials a feature needs, the hosts each one is
attached to and the stand-in it wants, `requiredOptions`, the options a user
has to set, and `context`, a sentence or two for the agent about what the
feature gives it, added to `~/BOTTLE.md` with `${option}` filled in).
Anything else in a definition is an error. Remote features aren't supported.

### `bottle new --spec FILE --name NAME [--git-repo PATH [--git-remote REMOTE]... [--checkout REF]] [--context FILE]`

Creates a bottle from a **spec**: a file giving its features and memory, owned
by whoever creates the bottle. `NAME` is required, and is the bottle's identity:
there's no default, and a name that's taken is refused. The image with those features
is built first if it isn't built yet.

```json
{"features": ["claude", "jvm:version=21", "tools"], "memory": "8G"}
```

`--git-repo PATH` is the repo whose objects are mounted read-only, so nothing
is cloned and the bottle can't change your repo except by pushing to a remote
it's served. It's checked out at `/workspace` at `--checkout`: a branch (the
repo's, else origin's) or a commit, by default the origin remote's default
branch, or, if there's no origin, whatever the repo has checked out. Without
`--git-repo` the bottle has an empty `/workspace`.

Each `--git-remote` serves the bottle one remote, `NAME:fetch=VIEW[,push=ns/NAMESPACE]`,
live, through its egress proxy. What a fetch sees is one of three views:

- `upstream`: your repo's upstream (its `origin/*` branches), read-only.
  Fetching first fetches your repo's `origin` on the host (at most once a
  minute), so the bottle gets the upstream's latest even if you haven't
  fetched.
- `heads`: your repo's own branches: how the agent sees what you've integrated.
- `ns/NAMESPACE`: the refs in one [git namespace](https://git-scm.com/docs/gitnamespaces)
  of your repo, and nothing else.

With `push`, pushes go into that namespace. A namespace is what confines the
bottle: every ref its `receive-pack` resolves is inside it, and your own refs
aren't advertised to it at all. Pushes are append-only, refusing force-pushes
and deletes, so nothing you've already read changes underneath you; no other
bottle may push into the same namespace; and your repo's own hooks don't run.
Reading one namespace and writing another makes a two-lane exchange, where
neither side's push is ever refused for what the other did: that's how
[folio](folio/README.md) gives each thread its lane. Without any
`--git-remote`, the bottle gets `origin` (`upstream`) and `host` (`heads`),
both read-only.

```sh
bottle new --spec spec.json --name foo-fix-tests --git-repo ~/src/foo --checkout main \
  --git-remote origin:fetch=upstream \
  --git-remote host:fetch=heads \
  --git-remote agent:fetch=ns/fix-tests-out,push=ns/fix-tests-out \
  --git-remote user:fetch=ns/fix-tests-in
```

`--context FILE` adds the file's contents to the end of `~/BOTTLE.md`, which
says what the bottle, its remotes and its features are, and nothing about how
to work in it.

Bottle keeps its state in `~/.bottle`; set `BOTTLE_HOME` to use another
directory.

### `bottle spec validate FILE [--json]`

Checks a spec as `bottle new --spec` would: its features exist, their options
are valid and every required one is set, and the memory fits this machine.
Prints it in canonical form, with `--json` as JSON: features sorted, each
one's options sorted and its defaults left out, and the memory as `all` or a
size.

### `bottle shell BOTTLE`

Opens a shell in the bottle at `/workspace`, starting the bottle if it's stopped.

### `bottle exec BOTTLE COMMAND [ARG]...`

Runs one command in the bottle, at `/workspace`, and exits with its status,
starting the bottle if it's stopped. Put `--` before the command if it takes
options of its own. Its output is a terminal only if bottle's is, so piping and
redirecting work.

```sh
bottle exec reponame -- claude -p 'fix the failing test'
bottle exec reponame cat /workspace/report.json | jq .failures
```

### `bottle status [BOTTLE]... [--json]`

Shows every bottle's state, or just those named; a name with no bottle shows as
`missing`. With `--json`, one object per bottle with what it was made from: its
repo, branch and commit, features, memory, spec and remotes.

### `bottle start BOTTLE` / `bottle stop BOTTLE`

Starts or stops a bottle's VM. Stopping keeps the checkout and any changes;
`shell` also starts a stopped bottle.

### `bottle delete BOTTLE`

Deletes the bottle and everything it created. If creating or deleting a bottle
was interrupted, `delete` cleans up whatever is left.

It doesn't ask: anything in the bottle that was never pushed goes with it, so
push what you want first. Whatever the bottle pushed is kept, in your repo's
namespaces. The bottle's image is kept, so the next
bottle with the same features starts without a build; images whose features
or base have changed since they were built are deleted, unless a bottle uses
one.

### `bottle shutdown`

Stops every running bottle and the anchor, then `bottled`.

The anchor is one small idle VM (about 290 MB of memory) attached to every
bottle's network. It works around an apple/container bug
([#2051](https://github.com/apple/container/issues/2051)) where restarting one
bottle could cut off another's network; bottle starts it whenever a bottle
needs it. If a bottle still reports that its network lost its bridge, run
`bottle shutdown && container system stop` and try again.

### `bottle daemon start` / `bottle daemon stop`

Starts or stops `bottled`, the background process that gives bottles network
access. It starts automatically when a command needs it, and restores network
access for every running bottle when it starts. While it's stopped, running
bottles have no network access.

### `bottle egress BOTTLE --listen HOST:PORT [--allow PATTERN]...`

Runs a bottle's egress proxy: an HTTP proxy (CONNECT and plain HTTP) that makes
connections from the host, so they use the host's DNS and VPN routes. Public
destinations are allowed. Private addresses are refused unless the hostname
matches an `--allow` pattern; the host's own loopback and link-local addresses
are always refused. Logs one line per connection. Bottle will start these
itself; the command exists for testing.

```sh
bottle egress mybottle --listen 192.168.128.1:3128 --allow '*.corp.example.com'
```

</details>

## Development

Run the tests from the repo root:

```sh
python3 -m unittest
```
