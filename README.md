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

## Development

Run the tests from the repo root:

```sh
python3 -m unittest
```
