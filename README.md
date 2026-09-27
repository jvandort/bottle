# bottle

## Prerequisites

- macOS 26+ on Apple silicon
- [Homebrew](https://brew.sh)
- Python 3.11+

## Usage

Run `bin/bottle`. It sets up what it depends on ([apple/container](https://github.com/apple/container),
its services, a Linux kernel) as necessary and asks before installing anything.

## Commands

### `bottle wrap [NAME] PATH`

Registers the git repo rooted at `PATH` under `NAME` in `~/.bottle/repos.json`, so
bottles can be created from it. Nothing is copied. `NAME` defaults to the origin
remote's repo name, else the directory name. Re-wrapping the same repo is a no-op.

```sh
bin/bottle wrap ~/path/to/reponame
bin/bottle wrap customname ~/path/to/reponame
```

Set `BOTTLE_HOME` to use a directory other than `~/.bottle`.

### `bottle build [--feature FEATURE]... [--no-cache]`

Builds the bottle image with the given features installed; `bottle new` does
this itself when needed.

```sh
bin/bottle build --feature tools --feature claude
```

Features, in the [Dev Container feature](https://containers.dev/implementors/features/)
format (a `devcontainer-feature.json` and an `install.sh` per directory):

- `tools`: a minimal working environment (ripgrep, fd, jq, curl, vim, tmux, ...).
- `jvm`: Eclipse Temurin JDKs, from Adoptium's signed apt repository. Options:
  `version` (default `25`), the default `java` and `JAVA_HOME`; and
  `additionalVersions`, more JDKs alongside it, e.g. `17,21`. Every JDK is in
  `/usr/lib/jvm` (also as `/usr/lib/jvm/jdk-<version>`), where tools like
  Gradle's toolchains find them.
- `claude`: Claude Code, from Anthropic's signed apt repository.

A feature can take options: `FEATURE:OPTION=VALUE[,OPTION=VALUE]`, e.g.
`--feature jvm:version=21,additionalVersions=17,11`. Options left out take
their defaults.

bottle runs features itself (no Dev Container tooling or Node) and supports a
subset of the format: `id`, `version`, metadata, `options` (string and boolean),
`containerEnv`, and `dependsOn` / `installsAfter` naming local features.
Anything else in a definition is an error. Remote features aren't supported.

### `bottle new REPO [--feature FEATURE]... [--branch BRANCH] [--name NAME]`

Creates a bottle: a VM with the given features, with `REPO`'s `BRANCH` checked out at
`/workspace`. `BRANCH` defaults to the origin remote's default branch, or, if
there's no origin, to whatever the repo has checked out (a branch or commit). The repo's history is
mounted read-only, so nothing is cloned and the bottle can't change your repo.
`NAME` defaults to the repo's name, then `REPO-2`, `REPO-3`, and so on. The image
with those features is built first if it isn't built yet.

```sh
bin/bottle new reponame --feature tools --feature jvm --feature claude
```

### `bottle shell NAME`

Opens a shell in the bottle at `/workspace`, starting the bottle if it's stopped.

### `bottle list`

Lists bottles and their state.

### `bottle git fetch NAME [REV] [--force]`

Fetches the bottle's git work into the repo it was created from, as
`bottle-NAME/<branch>` (listed by `git branch -r`). With no `REV`: every branch,
plus the bottle's `HEAD` as `bottle-NAME/detached/<commit>` if it's detached.
With a branch name: just that branch. With any other revision: that commit, into
`FETCH_HEAD`.

Fetching only adds: it creates and fast-forwards `bottle-NAME/*` refs, never
deletes them (a branch deleted in the bottle stays, and is reported as gone), and
never touches your own branches. If the bottle rewrote a branch's history, the
fetch refuses to overwrite it and says so; `--force` overwrites one named branch.

```sh
bin/bottle git fetch gradle
git log bottle-gradle/main
```

### `bottle start NAME` / `bottle stop NAME`

Starts or stops a bottle's VM. Stopping keeps the checkout and any changes;
`shell` also starts a stopped bottle.

### `bottle delete NAME [--force]`

Deletes the bottle and everything it created. If creating or deleting a bottle
was interrupted, `delete` cleans up whatever is left.

It refuses if the bottle has work its repo doesn't: commits that were never
fetched, or uncommitted changes. Fetch them first, or pass `--force`. Branches
already fetched into the repo (`bottle-NAME/*`) are kept.

### `bottle shutdown`

Stops every running bottle, then `bottled`.

### `bottle daemon start` / `bottle daemon stop`

Starts or stops `bottled`, the background process that gives bottles network
access. It starts automatically when a command needs it, and restores network
access for every running bottle when it starts. While it's stopped, running
bottles have no network access.

### `bottle egress NAME --listen HOST:PORT [--allow PATTERN]...`

Runs a bottle's egress proxy: an HTTP proxy (CONNECT and plain HTTP) that makes
connections from the host, so they use the host's DNS and VPN routes. Public
destinations are allowed. Private addresses are refused unless the hostname
matches an `--allow` pattern; the host's own loopback and link-local addresses
are always refused. Logs one line per connection. Bottle will start these
itself; the command exists for testing.

```sh
bin/bottle egress mybottle --listen 192.168.128.1:3128 --allow '*.corp.example.com'
```

## Development

Run the tests from the repo root:

```sh
python3 -m unittest
```
