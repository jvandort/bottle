"""Bottle images: Debian, then the base feature (the bottle contract), then a bottle's features.

An image is stages: ext4 disks, each what one build wrote on top of the stages
below it, stacked by overlayfs when a machine boots (see bottle-cli's build).
Machines on the same stages share them, and the host's cache of them: every
bottle's image is Debian's stage, base's on it, and its features' on that
(see features.py, which works out what to build).

A build is a recipe on a registry's image (pull: its reference) or on one of
bottle's built here (from: its tag): steps run in a VM, a directory of this
repo mounted read-only where they run, nothing of it copied in.

Built images are bottle-cli's, in its store where it runs (here, or a
runner), by their tags (`bottle-cli images`); labels are the stage's. A
stage's name is a hash of what it was built from, so rebuilding a tag never
replaces a stage a machine was made from; `bottle-cli gc` deletes those
nothing uses, keeping every tagged image and every machine's stages whatever
its state. folio names tags, and nothing of bottle-cli's store itself.
"""

import hashlib
import json
import shlex
import sys
from pathlib import Path

from bottle import krun
from bottle.errors import BottleError

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
# A step with this in its environment gets, from bottle-cli, the certificate of the caching CA builds share, for the
# https hosts their policy caches: the apt step (see apt.py). Every build's policy caches every feature's apt
# repositories (features.build_cached), so it's the same CA for every build, and a stage built again from the same is
# named the same.
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


def is_current(image: str, expected: str, cli: krun.Cli = krun.LOCAL) -> bool:
    """Whether `image` is built where `cli` runs bottle-cli from what it's built from now."""
    labels = built_labels(image, cli)
    return labels is not None and labels.get(INPUTS_LABEL) == expected


def interpreter(script: Path) -> list[str]:
    """How to run `script`, in its directory, mounted read-only: by its #! line, so it needn't be executable."""
    first = script.read_text().split("\n", 1)[0]
    return [*(shlex.split(first[2:]) if first.startswith("#!") else ["/bin/sh"]), f"./{script.name}"]


def build_recipe(context: Path, image: str, inputs: str, recipe: dict, labels: dict[str, str] | None = None,
                 cli: krun.Cli = krun.LOCAL) -> Path:
    """Build `recipe` into `image`, `context` mounted at its working directory (MOUNT): a stage, in a VM of
    bottle-cli's; return its disk.

    `recipe` is on a registry's image (pull: its reference) or on one of bottle's built here (from: its tag);
    its steps, entrypoint, cmd and labels are bottle-cli's (see bottle_disk::build::Recipe); its features, the
    specs of those it installs, are bottle's: the build reaches the public network, with none of their
    credentials, every feature's apt repositories kept (features.build_cached), and a step asking for BUILD_CA
    gets, from bottle-cli, the CA they're answered behind. `inputs` is the hash of what the image is built from, recorded as a label, with
    `labels`. Built where `cli` runs bottle-cli: here, or on a runner, the context sent there with the recipe.
    """
    from bottle import features, services  # features imports this module

    if "pull" in recipe:
        print(f"Pulling {recipe['pull']}...", file=sys.stderr)
    elif built_labels(recipe["from"], cli) is None:
        raise BottleError(f"{recipe['from']} isn't built")
    image_labels = {**recipe.get("labels", {}), INPUTS_LABEL: inputs, **(labels or {})}
    made = {**{k: recipe[k] for k in ("pull", "from", "entrypoint", "cmd") if k in recipe},
            "mount": {"target": MOUNT}, "steps": recipe.get("steps", []), "labels": image_labels}
    # The public network, with none of its features' credentials, nothing routed to folio serve; apt repositories
    # kept.
    network = {"cache": services.cache_rules(features.build_cached())}
    # Its progress on stderr; the disk of the stage it made, the last line of its stdout.
    result = cli.build(made, network, context, image)
    lines = result.stdout.split()
    if result.returncode != 0 or not lines:
        raise BottleError("bottle-cli image build failed")
    return Path(lines[-1])


# --- what's built -----------------------------------------------------------------


def _images(cli: krun.Cli = krun.LOCAL) -> list[dict]:
    """bottle-cli's images: each tag, its stage, disks and labels."""
    return json.loads(_cli(cli, "images", "--json"))


def _image(tag: str, cli: krun.Cli = krun.LOCAL) -> dict | None:
    return next((i for i in _images(cli) if i["tag"] == tag and i["disks"]), None)


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
        _cli(cli, "image", "rm", image)


def remove_unused(cli: krun.Cli = krun.LOCAL) -> None:
    """Delete the stages no image is made of and no machine uses: bottle-cli knows its images and machines."""
    _cli(cli, "gc")


def _cli(cli: krun.Cli, *args: str) -> str:
    result = cli.run(list(args))
    if result.returncode != 0:
        raise BottleError(f"bottle-cli {' '.join(args[:2])}: {result.stderr.strip()}")
    return result.stdout
