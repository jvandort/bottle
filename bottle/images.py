"""Bottle images, built from containers/images/<name>/ in this repo.

An image is stages: ext4 disks, each what one build wrote on top of the stages
below it, stacked by overlayfs when a machine boots (see bottle-cli's build).
Machines on the same stages share them, and the host's cache of them.

An image's directory holds an image.json, saying what it's built on and how it
runs, and an install.sh, run in a VM on what it's built on, with the
directory mounted read-only at its working directory, whatever's beside it
there to install:

  {"pull": "debian:trixie-slim",          a registry's image, by reference; or
   "from": "base",                        another of bottle's images
   "env": {"NAME": "value"},              set for install.sh and everything run in the image
   "entrypoint": [...], "cmd": [...],     what a machine runs when it starts; else what it's built on runs
   "labels": {"name": "value"}}

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

IMAGES = Path(__file__).resolve().parent.parent / "containers" / "images"
# The image every bottle starts from: the bottle contract. Not user-selectable;
# other images here would be alternative foundations meeting the same contract.
BASE = "base"
# What bottle's images are called: a tag names one in the index.
REPOSITORY = "localhost/bottle"
IMAGE_KEYS = {"pull", "from", "env", "entrypoint", "cmd", "labels"}
# Where a build's directory is in its VM: on /run's tmpfs, so nothing of it is on the stage.
MOUNT = "/run/bottle-build"
# What a build's steps run with, as a Dockerfile's RUN would: root's home, and no debconf prompts.
STEP_ENV = {"HOME": "/root", "DEBIAN_FRONTEND": "noninteractive"}


def available() -> list[str]:
    return sorted(p.parent.name for p in IMAGES.glob("*/image.json"))


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


def definition(name: str) -> dict:
    """The image's image.json, checked."""
    _require(name)
    where = f"containers/images/{name}/image.json"
    try:
        spec = json.loads((IMAGES / name / "image.json").read_text())
    except json.JSONDecodeError as e:
        raise BottleError(f"{where}: invalid JSON: {e}") from None
    if not isinstance(spec, dict):
        raise BottleError(f"{where}: must be a JSON object")
    unsupported = sorted(set(spec) - IMAGE_KEYS)
    if unsupported:
        raise BottleError(f"{where}: unsupported: {', '.join(unsupported)} (supported: {', '.join(sorted(IMAGE_KEYS))})")
    if ("pull" in spec) == ("from" in spec):
        raise BottleError(f"{where}: needs pull (a registry's image) or from (one of bottle's), not both")
    for key in ("pull", "from"):
        if key in spec and (not isinstance(spec[key], str) or not spec[key]):
            raise BottleError(f"{where}: {key} must be a string")
    for key in ("env", "labels"):
        value = spec.get(key, {})
        if not isinstance(value, dict) or not all(isinstance(v, str) for v in value.values()):
            raise BottleError(f"{where}: {key} must map names to strings")
    for key in ("entrypoint", "cmd"):
        value = spec.get(key)
        if value is not None and (not isinstance(value, list) or not all(isinstance(a, str) for a in value)):
            raise BottleError(f"{where}: {key} must be a list of strings")
    if not (IMAGES / name / "install.sh").is_file():
        raise BottleError(f"containers/images/{name}/install.sh is missing")
    return spec


def inputs_hash(name: str) -> str:
    """What the image `name` is built from: its directory, and the images it's built on."""
    digest = hashlib.sha256(tree_hash(IMAGES / name).encode())
    for dependency in dependencies(name):
        digest.update(inputs_hash(dependency).encode())
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


def dependencies(name: str) -> list[str]:
    """The bottle images `name` is built from, per its image.json's from."""
    spec = definition(name)
    return [spec["from"]] if "from" in spec else []


def build_order(name: str) -> list[str]:
    """`name` and everything it's built from, dependencies first."""
    order: list[str] = []

    def visit(image: str, path: list[str]) -> None:
        if image in path:
            raise BottleError(f"images depend on each other in a cycle: {' -> '.join([*path, image])}")
        if image in order:
            return
        if image not in available():
            if not path:
                _require(image)
            raise BottleError(f"{path[-1]} is built from {image}, but there's no containers/images/{image}")
        for dependency in dependencies(image):
            visit(dependency, [*path, image])
        order.append(image)

    visit(name, [])
    return order


def ensure_built(name: str, runner=None) -> bool:
    """Build `name`, and whatever it's built from, on `runner` if missing or stale there. True if anything was built."""
    built = False
    for image in build_order(name):
        if not is_current(tag(image), inputs_hash(image), runner):
            print(f"Building {tag(image)}{_where(runner)}...", file=sys.stderr)
            _build_one(image, runner=runner)
            built = True
    return built


def build(name: str, no_cache: bool = False, runner=None) -> str:
    """Build the named image on `runner`, first building anything it's built from that isn't built there yet."""
    for dependency in build_order(name)[:-1]:
        ensure_built(dependency, runner)
    _build_one(name, no_cache, runner)
    return tag(name)


def _where(runner) -> str:
    return f" on runner {runner.name}" if runner is not None and runner.name != "local" else ""


def _require(name: str) -> None:
    if name not in available():
        raise BottleError(f"no image named {name!r}; available: {', '.join(available())}")


def recipe(name: str) -> dict:
    """What builds image `name`: its image.json's, its install.sh as the step (see build_recipe)."""
    spec = definition(name)
    steps = [{"env": spec["env"]}] if spec.get("env") else []
    steps.append({"run": interpreter(IMAGES / name / "install.sh"), "env": STEP_ENV, "cwd": MOUNT})
    built = {"steps": steps, **{k: spec[k] for k in ("entrypoint", "cmd") if k in spec}}
    built["labels"] = spec.get("labels", {})
    return built | ({"pull": spec["pull"]} if "pull" in spec else {"from": tag(spec["from"])})


def interpreter(script: Path) -> list[str]:
    """How to run `script`, in its directory, mounted read-only: by its #! line, so it needn't be executable."""
    first = script.read_text().split("\n", 1)[0]
    return [*(shlex.split(first[2:]) if first.startswith("#!") else ["/bin/sh"]), f"./{script.name}"]


def _build_one(name: str, no_cache: bool = False, runner=None) -> None:
    _on(runner).build(IMAGES / name, tag(name), inputs_hash(name), recipe(name), no_cache)


def build_recipe(context: Path, image: str, inputs: str, recipe: dict, no_cache: bool = False,
                 labels: dict[str, str] | None = None) -> Path:
    """Build `recipe` into `image`, `context` mounted at its working directory (MOUNT): a stage, in a VM of
    bottle-cli's; return its disk.

    `recipe` is on a registry's image (pull: its reference) or on one of bottle's built here (from: its tag);
    its steps, entrypoint, cmd and labels are bottle-cli's (see bottle_disk::build::Recipe). `inputs` is the
    hash of what the image is built from, recorded as a label, with `labels`. With `no_cache`, the stage is
    made again even if one was made from the same.
    """
    from bottle import daemon  # daemon's auth imports features, which imports this module

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
    made = {"from": base.stem, "mount": {"source": str(context.resolve()), "target": MOUNT},
            "steps": recipe.get("steps", []), "labels": image_labels,
            **{k: recipe[k] for k in ("entrypoint", "cmd") if k in recipe}}
    builder = f"bottle-builder-{uuid.uuid4().hex[:12]}"
    try:
        with tempfile.NamedTemporaryFile("w", suffix=".json", prefix="recipe-") as file:
            json.dump(made, file)
            file.flush()
            # Bound with no features: the public network, no credentials, as a bottle would have it.
            decider, token = daemon.bind_decider(builder)
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
