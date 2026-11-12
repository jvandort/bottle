"""Bottle images: Debian, then the base feature (the bottle contract), then a bottle's features.

An image is stages: ext4 disks, each what one build wrote on top of the stages
below it, stacked by overlayfs when a machine boots (see bottle-cli's build).
Machines on the same stages share them, and the host's cache of them: every
bottle's image is Debian's stage, base's on it, and its features' on that
(see features.py, which works out what to build).

A build is a recipe on a registry's image (pull: its reference) or on one of
bottle's built here (from: its tag): steps run in a VM, a directory of this
repo mounted read-only where they run, nothing of it copied in.

Built images are stages in $BOTTLE_HOME/stages, bottle-cli's store, with an
index of each tag's top stage and labels in $BOTTLE_HOME/images.json. A
stage's name is a hash of what it was built from, so rebuilding a tag never
replaces a stage a machine was made from; `bottle-cli gc` deletes those
nothing uses, keeping every machine's whatever its state.
"""

import hashlib
import json
import os
import shlex
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

from bottle import krun, prereqs
from bottle.errors import BottleError
from bottle.store import bottle_home, read_json, write_json

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
# A step with this in its environment gets the certificate of the CA builds have (ca.BUILDS) in it: the apt step.
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
                 labels: dict[str, str] | None = None) -> Path:
    """Build `recipe` into `image`, `context` mounted at its working directory (MOUNT): a stage, in a VM of
    bottle-cli's; return its disk.

    `recipe` is on a registry's image (pull: its reference) or on one of bottle's built here (from: its tag);
    its steps, entrypoint, cmd and labels are bottle-cli's (see bottle_disk::build::Recipe); its features, the
    specs of those it installs, are bottle's: the build's connections are decided as those of a build of them
    (see daemon.bind_decider), their apt repositories kept, and a step asking for BUILD_CA gets the CA it's
    answered behind. `inputs` is the hash of what the image is built from, recorded as a label, with `labels`.
    With `no_cache`, the stage is made again even if one was made from the same.
    """
    from bottle import ca, daemon, features  # daemon's auth imports features, which imports this module

    prereqs.ensure_cli()
    store().mkdir(parents=True, exist_ok=True)
    if "pull" in recipe:
        print(f"Pulling {recipe['pull']}...", file=sys.stderr)
        base = _cli("pull", recipe["pull"], str(store()))
    else:
        disk = archive_of(recipe["from"])
        if disk is None:
            raise BottleError(f"{recipe['from']} isn't built")
        base = disk
    image_labels = {**recipe.get("labels", {}), INPUTS_LABEL: inputs, **(labels or {})}
    if no_cache:
        # A stage is named for what it's made from: something new in it makes it anew.
        image_labels["bottle.build"] = uuid.uuid4().hex
    certificate = ca.certificate(ca.BUILDS, features.apt_https_hosts()) or ""
    steps = [{**step, "env": {**step["env"], BUILD_CA: certificate}} if BUILD_CA in step.get("env", {}) else step
             for step in recipe.get("steps", [])]
    made = {"from": base.stem, "mount": {"source": str(context.resolve()), "target": MOUNT},
            "steps": steps, "labels": image_labels,
            **{k: recipe[k] for k in ("entrypoint", "cmd") if k in recipe}}
    builder = f"bottle-builder-{uuid.uuid4().hex[:12]}"
    try:
        with tempfile.NamedTemporaryFile("w", suffix=".json", prefix="recipe-") as file:
            json.dump(made, file)
            file.flush()
            # The public network, with none of its features' credentials; their apt repositories kept.
            decider, token = daemon.bind_decider(builder, features=recipe.get("features", []), build=True)
            disk = _cli("build", file.name, str(store()), "--decider", decider, env={"BOTTLE_DECIDER_TOKEN": token})
    finally:
        try:
            daemon.release_egress(builder)
        except BottleError as e:
            print(f"bottle: cleaning up {builder}: {e}", file=sys.stderr)
    index = read_json(_index_path()) or {}
    index[image] = {"disk": disk.name, "labels": image_labels}
    write_json(_index_path(), index)
    return disk


def _cli(*args: str, env: dict[str, str] | None = None) -> Path:
    """Run a bottle-cli build, pull or import, its progress on stderr; the disk of the stage it made (its last line)."""
    result = subprocess.run([krun.binary(), *args], stdout=subprocess.PIPE, text=True,
                            env={**os.environ, **env} if env else None)
    lines = result.stdout.split()
    if result.returncode != 0 or not lines:
        raise BottleError(f"bottle-cli {args[0]} {args[1]} failed")
    return Path(lines[-1])


# --- the store -----------------------------------------------------------------


def store() -> Path:
    """bottle-cli's store of stages."""
    return bottle_home() / "stages"


def _index_path() -> Path:
    return bottle_home() / "images.json"


def archive_of(image: str) -> Path | None:
    """The disk of the top stage `image` (a tag) was last built into, or None."""
    entry = (read_json(_index_path()) or {}).get(image)
    return store() / entry["disk"] if entry and (store() / entry["disk"]).exists() else None


def built_labels(image: str) -> dict[str, str] | None:
    """The labels `image` was last built with, or None if it's not built."""
    entry = (read_json(_index_path()) or {}).get(image)
    return dict(entry["labels"]) if entry and archive_of(image) else None


def built() -> dict[str, dict[str, str]]:
    """Every built tag, with its labels."""
    return {image: dict(entry["labels"]) for image, entry in (read_json(_index_path()) or {}).items()}


def forget(image: str) -> None:
    """Drop `image` from the index; its stages go once nothing uses them (remove_unused)."""
    index = read_json(_index_path()) or {}
    if index.pop(image, None) is not None:
        write_json(_index_path(), index)


def remove_unused(in_use: set[str]) -> None:
    """Delete the stages no tag's image is made of and no machine uses (bottle-cli knows its machines; `in_use`,
    the disks bottles were made from by name, are kept too)."""
    if not store().is_dir():
        return
    keep = {entry["disk"] for entry in (read_json(_index_path()) or {}).values()} | in_use
    kept = [str(store() / name) for name in sorted(keep) if (store() / name).exists()]
    result = subprocess.run([krun.binary(), "gc", str(store()), *kept], capture_output=True, text=True)
    if result.returncode != 0:
        raise BottleError(f"bottle-cli gc: {result.stderr.strip()}")
