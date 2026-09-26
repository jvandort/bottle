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

Images build on each other, so build `base` before `tools`:

```sh
bin/bottle build base
bin/bottle build tools
```

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
