"""Bottle images, built from containers/images/<name>/ in this repo.

smolvm boots images but doesn't build them, so bottle builds them on top of
smolvm's machines: buildah runs in a machine made for the build, its inputs
sent in over exec's stdin, and the archive it pushes comes back over stdout.
The builder's connections are decided like a bottle's, with no credentials.

Built images are OCI archives in $BOTTLE_HOME/images, with an index of each
tag's archive and labels. An archive's name carries its inputs' hash, so
rebuilding a tag never replaces an archive a machine was made from: smolvm
reads it again on every start.
"""

import hashlib
import os
import re
import shlex
import subprocess
import sys
import tarfile
import uuid
from pathlib import Path

from bottle import prereqs, smolvm
from bottle.errors import BottleError
from bottle.store import bottle_home, read_json, write_json

IMAGES = Path(__file__).resolve().parent.parent / "containers" / "images"
# The image every bottle starts from: the bottle contract. Not user-selectable;
# other images here would be alternative foundations meeting the same contract.
BASE = "base"
# Where bottle's images are named: on no registry. A name without a host in it
# (bottle/base) is taken to be Docker Hub's; localhost is a host, so a
# Dockerfile's FROM finds the image built here.
REPOSITORY = "localhost/bottle"
# A stage built on another bottle image, e.g. `FROM localhost/bottle/base:latest`.
FROM_BOTTLE = re.compile(r"^\s*FROM\s+(?:--\S+\s+)*localhost/bottle/([A-Za-z0-9._-]+)(?::latest)?(?:\s|$)", re.MULTILINE | re.IGNORECASE)


def available() -> list[str]:
    return sorted(p.parent.name for p in IMAGES.glob("*/Dockerfile"))


# Every image bottle builds records a hash of what it was built from. An image
# whose inputs have changed since is stale: it's rebuilt when next needed, and
# deleted once no container uses it.
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


def inputs_hash(name: str) -> str:
    """What the image `name` is built from: its directory, and the images it's built on."""
    _require(name)
    digest = hashlib.sha256(tree_hash(IMAGES / name).encode())
    for dependency in dependencies(name):
        digest.update(inputs_hash(dependency).encode())
    return digest.hexdigest()


def is_current(image: str, expected: str) -> bool:
    labels = built_labels(image)
    return labels is not None and labels.get(INPUTS_LABEL) == expected


def dependencies(name: str) -> list[str]:
    """The bottle images `name` is built from, per its Dockerfile's FROM lines."""
    _require(name)
    return list(dict.fromkeys(FROM_BOTTLE.findall((IMAGES / name / "Dockerfile").read_text())))


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
            raise BottleError(f"{path[-1]} is built from {REPOSITORY}/{image}, but there's no containers/images/{image}")
        for dependency in dependencies(image):
            visit(dependency, [*path, image])
        order.append(image)

    visit(name, [])
    return order


def ensure_built(name: str) -> bool:
    """Build `name`, and whatever it's built from, if missing or stale. True if anything was built."""
    built = False
    for image in build_order(name):
        if not is_current(tag(image), inputs_hash(image)):
            print(f"Building {tag(image)}...", file=sys.stderr)
            _build_one(image)
            built = True
    return built


def build(name: str, no_cache: bool = False) -> str:
    """Build the named image, first building anything it's built from that isn't built yet."""
    for dependency in build_order(name)[:-1]:
        ensure_built(dependency)
    _build_one(name, no_cache)
    return tag(name)


def _require(name: str) -> None:
    if name not in available():
        raise BottleError(f"no image named {name!r}; available: {', '.join(available())}")


def _build_one(name: str, no_cache: bool = False) -> None:
    _require(name)
    build_context(IMAGES / name, tag(name), inputs_hash(name), no_cache)


def build_context(
    context: Path, image: str, inputs: str, no_cache: bool = False, dockerfile: Path | None = None,
    labels: dict[str, str] | None = None,
) -> None:
    """Build `context` into `image`, in a builder machine (see _build).

    `inputs` is the hash of what the image is built from, recorded as a label,
    with `labels`. The bottle images its Dockerfile is built FROM are loaded
    into the builder first.
    """
    prereqs.ensure_smolvm()
    bases = {}
    for name in FROM_BOTTLE.findall((dockerfile or context / "Dockerfile").read_text()):
        archive = archive_of(tag(name))
        if archive is None:
            raise BottleError(f"{tag(name)} isn't built")
        bases[tag(name)] = archive
    _build(context, image, {INPUTS_LABEL: inputs, **(labels or {})}, no_cache, dockerfile, bases)


# --- the store -----------------------------------------------------------------


def store() -> Path:
    return bottle_home() / "images"


def _index_path() -> Path:
    return store() / "index.json"


def archive_of(image: str) -> Path | None:
    """The archive `image` (a tag) was last built into, or None."""
    entry = (read_json(_index_path()) or {}).get(image)
    return store() / entry["archive"] if entry and (store() / entry["archive"]).exists() else None


def built_labels(image: str) -> dict[str, str] | None:
    """The labels `image` was last built with, or None if it's not built."""
    entry = (read_json(_index_path()) or {}).get(image)
    return dict(entry["labels"]) if entry and archive_of(image) else None


def built() -> dict[str, dict[str, str]]:
    """Every built tag, with its labels."""
    return {image: dict(entry["labels"]) for image, entry in (read_json(_index_path()) or {}).items()}


def forget(image: str) -> None:
    """Drop `image` from the index; its archive goes once nothing uses it (remove_unused)."""
    index = read_json(_index_path()) or {}
    if index.pop(image, None) is not None:
        write_json(_index_path(), index)


def remove_unused(in_use: set[str]) -> None:
    """Delete archives no tag points to and no machine was made from (`in_use`, archive names)."""
    current = {entry["archive"] for entry in (read_json(_index_path()) or {}).values()}
    for path in store().glob("*.tar") if store().is_dir() else ():
        if path.name not in current | in_use:
            path.unlink()


# --- building --------------------------------------------------------------------

# The builder: buildah, in a machine made for each build and gone after it.
BUILDER_IMAGE = "quay.io/buildah/stable"
# Where the build's inputs go in the builder, sent as a tar over exec's stdin:
# the context, the Dockerfile, and the bottle images a Dockerfile is built FROM.
INPUTS = "/build"
# Where buildah pushes the archive, in the builder, for exec to carry out.
OUT = "/tmp/bottle-image.tar"


def _build(context: Path, image: str, image_labels: dict[str, str], no_cache: bool = False,
           dockerfile: Path | None = None, bases: dict[str, Path] | None = None) -> Path:
    """Build `context` into an archive for `image`, in a builder machine; return the archive.

    `bases` are the bottle images the Dockerfile is built FROM, by tag, and
    the archives they're in: loaded into the builder first, under those tags.
    Everything goes in and out over exec, so the builder needs no mounts.
    """
    from bottle import daemon  # daemon's auth imports features, which imports this module

    inputs = image_labels.get("bottle.inputs", "")[:12] or "unlabelled"
    name = f"{re.sub(r'[^A-Za-z0-9._-]+', '_', image)}-{inputs}.tar"
    store().mkdir(parents=True, exist_ok=True)
    buildah = "buildah --storage-driver vfs"
    # Progress on stderr, the user's to see as a build's is; stdout is for the archive.
    script = ["set -e", "exec >&2"]
    for base, path in (bases or {}).items():
        script.append(f"id=$({buildah} pull -q oci-archive:{INPUTS}/bases/{shlex.quote(path.name)})")
        script.append(f'{buildah} tag "$id" {shlex.quote(base)}')
    build_args = [f"--label {shlex.quote(f'{k}={v}')}" for k, v in image_labels.items()]
    script.append(
        f"{buildah} build --isolation chroot {'--no-cache ' if no_cache else ''}{' '.join(build_args)} "
        f"-t {shlex.quote(image)} -f {INPUTS}/Dockerfile {INPUTS}/context"
    )
    script.append(f"{buildah} push {shlex.quote(image)} oci-archive:{OUT}:{shlex.quote(image)}")

    builder = f"bottle-builder-{uuid.uuid4().hex[:12]}"
    sent = bottle_home() / "build" / f"{builder}.tar"
    partial = store() / f".{name}.partial"
    try:
        sent.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(sent, "w") as tar:
            tar.add(context, "context")
            tar.add(dockerfile or context / "Dockerfile", "Dockerfile")
            for path in (bases or {}).values():
                tar.add(path, f"bases/{path.name}")
        smolvm.create(builder, BUILDER_IMAGE, mounts=[], env={}, memory_mib=None, cpus=os.cpu_count() or 1)
        # Bound with no features: the public network, no credentials, as a bottle would have it.
        decider, token = daemon.bind_decider(builder)
        smolvm.start(builder, decider, token)
        _in_builder(builder, ["sh", "-c", f"mkdir -p {INPUTS} && tar -x -C {INPUTS}"], f"sending {image}'s inputs",
                    stdin=sent)
        _in_builder(builder, ["sh", "-c", "\n".join(script)], f"building {image}")
        with partial.open("wb") as out:
            _in_builder(builder, ["cat", OUT], f"reading {image} out of the builder", stdout=out)
        partial.replace(store() / name)
    finally:
        sent.unlink(missing_ok=True)
        partial.unlink(missing_ok=True)
        for cleanup in (lambda: daemon.release_egress(builder), lambda: smolvm.delete(builder)):
            try:
                cleanup()
            except BottleError as e:
                print(f"bottle: cleaning up {builder}: {e}", file=sys.stderr)
    index = read_json(_index_path()) or {}
    index[image] = {"archive": name, "labels": image_labels}
    write_json(_index_path(), index)
    return store() / name


def _in_builder(builder: str, argv: list[str], what: str, stdin: Path | None = None, stdout=None) -> None:
    """Run `argv` in the builder, `stdin` a file to give it; stderr goes to the user."""
    with stdin.open("rb") if stdin else open(os.devnull, "rb") as source:
        if subprocess.run(smolvm.exec_command(builder, argv), stdin=source, stdout=stdout).returncode != 0:
            raise BottleError(f"{what} failed")
