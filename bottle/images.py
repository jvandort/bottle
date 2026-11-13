"""Bottle images: Debian, then the base feature (the bottle contract), then a bottle's features.

An image is stages: ext4 disks, each what one build wrote on top of the stages
below it, stacked by overlayfs when a machine boots (see bottle-cli's build).
Machines on the same stages share them, and the host's cache of them: every
bottle's image is Debian's stage, base's on it, and its features' on that
(see features.py, which works out what to build).

A build is a recipe on a registry's image (pull: its reference) or on one of
bottle's built here (from: its tag): steps run in a VM, a directory of this
repo mounted read-only where they run, nothing of it copied in.

Built images are stages in $BOTTLE_HOME/stages, bottle-cli's store, which
keeps each tag's top stage too (`bottle-cli images`); labels are the stage's.
A stage's name is a hash of what it was built from, so rebuilding a tag never
replaces a stage a machine was made from; `bottle-cli gc` deletes those
nothing uses, keeping every tagged image and every machine's stages whatever
its state.
"""

import hashlib
import json
import shlex
import subprocess
import sys
import uuid
from pathlib import Path

from bottle import krun, prereqs
from bottle.errors import BottleError
from bottle.store import bottle_home, read_json

# The image every bottle is: the base feature, the bottle contract, on Debian. Not user-selectable.
BASE = "base"
# Debian 13 by name, not by digest: a build takes trixie's latest point release
# and security fixes, but a new Debian release is a change to this line. Its apt
# sources name trixie too, so later installs stay on the same release.
DEBIAN = "debian:trixie-slim"
# What a bottle runs when it starts: the base feature's entrypoint, under tini (a subreaper, -s: it isn't PID 1),
# and sshd. Part of the contract bottle relies on (CONTRACT in bottles.py).
ENTRYPOINT = ["/usr/bin/tini", "-s", "--", "/usr/local/bin/bottle-entrypoint"]
CMD = ["/usr/sbin/sshd", "-D", "-e"]
# What bottle's images are called: a tag names one in the index.
REPOSITORY = "localhost/bottle"
# Where a build's directory is in its VM: on /run's tmpfs, so nothing of it is on the stage.
MOUNT = "/run/bottle-build"
# What a build's steps run with, as a Dockerfile's RUN would: root's home, and no debconf prompts.
STEP_ENV = {"HOME": "/root", "DEBIAN_FRONTEND": "noninteractive"}
# The caching CA every image's build has, bottle-cli's (bottle-cli ca), for its apt step (see apt.py): constrained to
# the https apt repositories every feature declares, so it's the same for every build, and a stage built again from
# the same is named the same.
BUILDS_CA = "bottle-builds"
# A step with this in its environment gets the certificate of the CA builds have (BUILDS_CA) in it: the apt step.
BUILD_CA = "BOTTLE_BUILD_CA"


# Every image bottle builds records a hash of what it was built from. An image
# whose inputs have changed since is stale: it's rebuilt when next needed, and
# deleted once no machine uses it.
INPUTS_LABEL = "bottle.inputs"


def tag(name: str) -> str:
    return f"{REPOSITORY}/{name}:latest"


def tree_hash(path: Path) -> str:
    """A hash of every file under `path`: names, contents and whether each is executable."""
    digest = hashlib.sha256()
    for file in sorted(p for p in path.rglob("*") if p.is_file()):
        digest.update(f"{file.relative_to(path)}\0{file.stat().st_mode & 0o111:o}\0".encode())
        digest.update(hashlib.sha256(file.read_bytes()).digest())
    return digest.hexdigest()


def is_current(image: str, expected: str, runner=None) -> bool:
    """Whether `image` is built on `runner` (default: this machine) from what it's built from now."""
    labels = _on(runner).image_labels(image)
    return labels is not None and labels.get(INPUTS_LABEL) == expected


def _on(runner):
    """`runner`, or the local one: where images are kept and built."""
    if runner is not None:
        return runner
    from bottle import runners  # runners imports this module

    return runners.get()


def _where(runner) -> str:
    return f" on runner {runner.name}" if runner is not None and runner.name != "local" else ""


def interpreter(script: Path) -> list[str]:
    """How to run `script`, in its directory, mounted read-only: by its #! line, so it needn't be executable."""
    first = script.read_text().split("\n", 1)[0]
    return [*(shlex.split(first[2:]) if first.startswith("#!") else ["/bin/sh"]), f"./{script.name}"]


def build_recipe(context: Path, image: str, inputs: str, recipe: dict, no_cache: bool = False,
                 labels: dict[str, str] | None = None, cli: krun.Cli = krun.LOCAL) -> Path:
    """Build `recipe` into `image`, `context` mounted at its working directory (MOUNT): a stage, in a VM of
    bottle-cli's; return its disk.

    `recipe` is on a registry's image (pull: its reference) or on one of bottle's built here (from: its tag);
    its steps, entrypoint, cmd and labels are bottle-cli's (see bottle_disk::build::Recipe); its features, the
    specs of those it installs, are bottle's: the build reaches the public network, with none of their
    credentials, their apt repositories kept (cache.cached_for, a build's), and a step asking for BUILD_CA gets
    the CA they're answered behind. `inputs` is the hash of what the image is built from, recorded as a label, with `labels`.
    With `no_cache`, the stage is made again even if one was made from the same. Built where `cli` runs
    bottle-cli: here, or on a runner, the context sent there with the recipe.
    """
    from bottle import cache, features, services  # features imports this module

    if cli is krun.LOCAL:
        prereqs.ensure_cli()
        _migrate()
    if "pull" in recipe:
        print(f"Pulling {recipe['pull']}...", file=sys.stderr)
    elif archive_of(recipe["from"], cli) is None:
        raise BottleError(f"{recipe['from']} isn't built")
    image_labels = {**recipe.get("labels", {}), INPUTS_LABEL: inputs, **(labels or {})}
    if no_cache:
        # A stage is named for what it's made from: something new in it makes it anew.
        image_labels["bottle.build"] = uuid.uuid4().hex
    # The caching CA of bottle-cli's builds, which answers their apt repositories.
    certificate = krun.ca_certificate(BUILDS_CA, features.apt_https_hosts(), cli=cli) or ""
    steps = [{**step, "env": {**step["env"], BUILD_CA: certificate}} if BUILD_CA in step.get("env", {}) else step
             for step in recipe.get("steps", [])]
    made = {**{k: recipe[k] for k in ("pull", "from", "entrypoint", "cmd") if k in recipe},
            "mount": {"target": MOUNT}, "steps": steps, "labels": image_labels}
    # The public network, with none of its features' credentials, nothing routed to bottled; their apt
    # repositories kept.
    network = {"cache": services.cache_rules(cache.cached_for(recipe.get("features", []), build=True))}
    # Its progress on stderr; the disk of the stage it made, the last line of its stdout.
    result = cli.build(made, network, context, str(store(cli)), image)
    lines = result.stdout.split()
    if result.returncode != 0 or not lines:
        raise BottleError("bottle-cli image build failed")
    return Path(lines[-1])


# --- the store -----------------------------------------------------------------


def store(cli: krun.Cli = krun.LOCAL) -> Path:
    """bottle-cli's store of stages where `cli` runs it, which keeps its images' tags too."""
    return Path(cli.home()) / "stages"


def _images(cli: krun.Cli = krun.LOCAL) -> list[dict]:
    """bottle-cli's images: each tag, its stage, disks and labels."""
    if cli is krun.LOCAL:
        if not store().is_dir():
            return []
        _migrate()
    return json.loads(_cli(cli, "images", str(store(cli)), "--json"))


def _image(tag: str, cli: krun.Cli = krun.LOCAL) -> dict | None:
    return next((i for i in _images(cli) if i["tag"] == tag and i["disks"]), None)


def archive_of(image: str, cli: krun.Cli = krun.LOCAL) -> Path | None:
    """The disk of the top stage `image` (a tag) was last built into, or None."""
    found = _image(image, cli)
    return Path(found["disks"][-1]) if found else None


def built_labels(image: str, cli: krun.Cli = krun.LOCAL) -> dict[str, str] | None:
    """The labels `image` was last built with, or None if it's not built."""
    found = _image(image, cli)
    return dict(found["labels"]) if found else None


def built(cli: krun.Cli = krun.LOCAL) -> dict[str, dict[str, str]]:
    """Every built tag, with its labels."""
    return {i["tag"]: dict(i["labels"]) for i in _images(cli)}


def forget(image: str, cli: krun.Cli = krun.LOCAL) -> None:
    """Untag `image`; its stages go once nothing uses them (remove_unused)."""
    if any(i["tag"] == image for i in _images(cli)):
        _cli(cli, "image", "rm", image, str(store(cli)))


def remove_unused(in_use: set[str], cli: krun.Cli = krun.LOCAL) -> None:
    """Delete the stages no image is made of and no machine uses (bottle-cli knows its images and machines;
    `in_use`, the disks bottles were made from by name, are kept too)."""
    if cli is krun.LOCAL and not store().is_dir():
        return
    _cli(cli, "gc", str(store(cli)), *(str(store(cli) / name) for name in sorted(in_use)))


def _cli(cli: krun.Cli, *args: str) -> str:
    result = cli.run(list(args))
    if result.returncode != 0:
        raise BottleError(f"bottle-cli {' '.join(args[:2])}: {result.stderr.strip()}")
    return result.stdout


def _migrate() -> None:
    """The tags bottle kept itself, in $BOTTLE_HOME/images.json, given to bottle-cli's store, once."""
    index = bottle_home() / "images.json"
    entries = read_json(index)
    if entries is None:
        return
    for tag, entry in entries.items():
        if (store() / entry["disk"]).exists():
            subprocess.run([krun.binary(), "image", "tag", Path(entry["disk"]).stem, tag, str(store())],
                           capture_output=True)
    index.unlink()
