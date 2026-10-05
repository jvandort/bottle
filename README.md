# Bottle

*The perfect home for a genie.*

Bottle manages sandboxed Linux VMs for coding agents, on macOS. Let the agent run with full
permissions inside. Take back only the commits you want.

- **A real VM per bottle.** Its own kernel, via [apple/container](https://github.com/apple/container),
  with everything it needs already installed. Starts in about a second.
- **Your repo, live — and nothing else of yours.** The bottle checks out your history from a
  read-only mount of the repo's git objects, and fetches your upstream through the host. Your
  working copy, and the rest of your machine, are never mounted.
- **Git in, git out.** The repo is mounted read-only, and each bottle is a git remote you
  fetch from and push to. A bottle writes its own [namespace](https://git-scm.com/docs/gitnamespaces)
  and nothing else, can't touch your branches, and can't rewrite what you've already read.
- **Network on a leash.** Egress only through a proxy on your machine, so a bottle gets your DNS and
  VPN routes but not your local network, even as root.
- **Credentials that stay on your machine.** A feature can name the hosts its token belongs to, and
  the proxy attaches it there. The bottle talks to the proxy and never holds the token.
- **Composable features.** `tools`, `jvm`, `python`, `claude`, `github`, `teamcity`, in the
  [Dev Container feature](https://containers.dev/implementors/features/) format, with per-repo
  defaults. Images are prebuilt, and rebuilt when they go stale.
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

```sh
bottle repo add ~/path/to/foo --feature tools --feature claude
bottle new foo            # Create a bottle with foo's default features
bottle shell foo          # Open a shell at /workspace; run `claude` here
git fetch bottle-foo      # In your repo: see what the agent pushed
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
| `bottle repo add` / `list` / `set` / `update` | Register a repo, and set its bottles' default features |
| `bottle auth login` / `list` / `set` / `logout` | Store a credential once, for every bottle |
| `bottle new` | Create a bottle from a repo |
| `bottle shell` | Open a shell in a bottle, at `/workspace` |
| `bottle exec` | Run one command in a bottle |
| `bottle list` | List bottles and their state |
| `bottle start` / `stop` / `reset` / `delete` | Control a bottle's lifecycle |
| `bottle build` | Build an image with the given features |
| `bottle daemon start` / `stop` | Manage `bottled`, the background network process |
| `bottle shutdown` | Stop every running bottle, then `bottled` |
| `bottle egress` | Run a bottle's egress proxy by hand, for testing |

<details>
<summary><b>Full reference</b> — every command, its arguments and its rules.</summary>

### `bottle repo add [NAME] PATH [--feature FEATURE]...`

Registers the git repo rooted at `PATH` under `NAME`, so bottles can be created
from it. Nothing is copied. `NAME` defaults to the origin remote's repo name,
else the directory name. The features become the defaults for the repo's
bottles. Adding the same repo again is a no-op.

```sh
bottle repo add ~/path/to/reponame --feature tools --feature jvm:version=25,additionalVersions=17,21 --feature claude
```

### `bottle repo list`

Lists repos and their default features.

### `bottle repo set REPO [--feature FEATURE]...`

Replaces all the repo's settings: its default features become exactly those
given. Existing bottles get them when reset.

### `bottle repo update REPO --feature FEATURE...`

Adds features to the repo's existing ones: a feature given again replaces its
options, others are kept. Existing bottles get them when reset.

```sh
bottle repo update foo --feature github
```

Repos are stored in `~/.bottle/repos.json`, which may be edited by hand. Set
`BOTTLE_HOME` to use a directory other than `~/.bottle`.

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

`bottle new`, `shell`, `start` and `reset` log in for you when a bottle's
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
  removed, since a bottle is disposable and `bottle reset` undoes whatever an
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
  doesn't pass through your repo the way a bottle's `work` lane does, so give
  it just the repositories and permissions the agent needs. GitHub's ssh
  remotes (`git@github.com:...`) are fetched over https instead, since a bottle
  has no ssh key.

  ```sh
  bottle new reponame --feature tools --feature github
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
  bottle new reponame --feature tools --feature teamcity:server=https://teamcity.example.com
  bottle exec reponame -- teamcity run list
  ```

A feature can take options: `FEATURE:OPTION=VALUE[,OPTION=VALUE]`, e.g.
`--feature jvm:version=21,additionalVersions=17,11`. Options left out take
their defaults.

bottle runs features itself (no Dev Container tooling or Node) and supports a
subset of the format: `id`, `version`, metadata, `options` (string and boolean),
`containerEnv`, `dependsOn` / `installsAfter` naming local features, and
`customizations.bottle` (the credentials a feature needs, the hosts each one is
attached to and the stand-in it wants, and `requiredOptions`, the options a user
has to set).
Anything else in a definition is an error. Remote features aren't supported.

### `bottle new REPO [--feature FEATURE]... [--branch BRANCH] [--name NAME]`

Creates a bottle: a VM with `REPO`'s `BRANCH` checked out at `/workspace`,
and the repo's default features plus any given (a feature given again replaces
that default's options). `BRANCH` defaults to the origin remote's default
branch, or, if there's no origin, to whatever the repo has checked out (a branch
or commit). The repo's history is mounted read-only, so nothing is cloned and
the bottle can't change your repo, except through `host` below. The bottle has
three remotes, served live by bottle through its egress proxy:

- `origin`: your repo's upstream (its `origin/*` branches), read-only. Fetching
  first fetches your repo's `origin` on the host (at most once a minute), so
  the bottle gets the upstream's latest even if you haven't fetched.
- `host`: your repo's own branches, read-only. How the agent sees what you've
  integrated: `git fetch host && git rebase host/main`.
- `work`: where you and the agent exchange commits. Each of you writes a
  separate [git namespace](https://git-scm.com/docs/gitnamespaces) and reads the
  other's, so neither side's push is ever refused because of what the other
  did — you can commit an edit, find the agent has since pushed something that
  conflicts, and push anyway. The agent resolves the conflict and your next pull is a
  fast-forward. A namespace is also what confines the bottle: every ref its
  `receive-pack` resolves is inside one, and your own refs aren't advertised to
  it at all. Its lane refuses force-pushes and deletes, so nothing you've
  already read changes underneath you. Your repo's own hooks don't run.

`bottle new` registers the bottle in your repo as a remote called
`bottle-NAME`, so from your side it is an ordinary one: `git fetch bottle-NAME`,
`git switch BRANCH` to start tracking what it pushed, `git pull`, `git push`.

`NAME` defaults to the repo's name, then `REPO-2`, `REPO-3`, and so on. The image
with those features is built first if it isn't built yet.

```sh
bottle new reponame --feature tools --feature jvm --feature claude
```

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

### `bottle list`

Lists bottles and their state.

### `bottle start BOTTLE` / `bottle stop BOTTLE`

Starts or stops a bottle's VM. Stopping keeps the checkout and any changes;
`shell` also starts a stopped bottle.

### `bottle delete BOTTLE [--force]`

Deletes the bottle and everything it created. If creating or deleting a bottle
was interrupted, `delete` cleans up whatever is left.

It refuses if the bottle has work its repo doesn't: commits that were never
pushed, or uncommitted changes. Push them first (`bottle exec NAME git push
work`), or pass `--force`. Whatever the bottle pushed is kept, since it may be
the only copy; the remote is removed. The bottle's image is deleted too, unless
another bottle uses it.

### `bottle reset BOTTLE [--force]`

Deletes the bottle and creates it again with the arguments `bottle new` was
given: a fresh, running VM with the repo's current default features (plus any
the bottle was created with), and `/workspace` at the latest commit of its
branch. Like `delete`, it refuses to lose unpushed commits or uncommitted
changes without `--force`. Also recreates a bottle whose VM has gone missing,
and registers its `bottle-NAME` remote again if the repo has lost it.

### `bottle shutdown`

Stops every running bottle, then `bottled`.

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
