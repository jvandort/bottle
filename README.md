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

### `bottle build IMAGE [--no-cache]`

Builds the image defined in `containers/IMAGE` and tags it `bottle/IMAGE:latest`.
Images can build on each other (`FROM bottle/base:latest`); any dependency that `IMAGE`
is built from and that aren't built yet are built first.

```sh
bin/bottle build tools
```

### `bottle new REPO --image IMAGE [--branch BRANCH] [--name NAME]`

Creates a bottle: a VM running `IMAGE`, with `REPO`'s `BRANCH` checked out at
`/workspace`. `BRANCH` defaults to the origin remote's default branch, or, if
there's no origin, to whatever the repo has checked out (a branch or commit). The repo's history is
mounted read-only, so nothing is cloned and the bottle can't change your repo.
`NAME` defaults to the repo's name, then `REPO-2`, `REPO-3`, and so on. Builds
`IMAGE` first if it isn't built yet.

```sh
bin/bottle new gradle --image tools
```

### `bottle shell NAME`

Opens a shell in the bottle at `/workspace`, starting the bottle if it's stopped.

### `bottle list`

Lists bottles and their state.

### `bottle start NAME` / `bottle stop NAME`

Starts or stops a bottle's VM. Stopping keeps the checkout and any changes;
`shell` also starts a stopped bottle.

### `bottle delete NAME`

Deletes the bottle and everything it created. If creating or deleting a bottle
was interrupted, `delete` cleans up whatever is left.

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
