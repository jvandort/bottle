"""Specs: a bottle's environment -- its features and its memory -- as a file.

    {"features": ["claude", "jvm:version=25", "tools"], "memory": "8G"}

Bottle keeps no specs. Whoever creates a bottle owns the file and passes it to
`bottle new --spec`; `bottle spec validate` checks one the same way, and gives
its canonical form: each feature's options sorted and every default left
out, features sorted, and the memory as `container run` takes it, or `all`.
A feature's dependencies aren't written in: they're the feature's to declare,
and resolved again whenever a bottle is created.
"""

import json
from pathlib import Path

from bottle import repos
from bottle.errors import BottleError

KEYS = ("features", "memory")


def read(path: Path) -> dict:
    """The canonical form of the spec in the file at `path`."""
    try:
        text = path.read_text()
    except OSError as e:
        raise BottleError(f"can't read spec {path}: {e.strerror or e}") from None
    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        raise BottleError(f"spec {path} isn't valid JSON: {e}") from None
    try:
        return canonical(data)
    except BottleError as e:
        raise BottleError(f"spec {path}: {e}") from None


def canonical(data: object) -> dict:
    """Validate a spec and put it in canonical form, as `bottle new` would accept it."""
    from bottle import features  # imports images and runtime, which loading a spec doesn't otherwise need

    if not isinstance(data, dict):
        raise BottleError('a spec is a JSON object, e.g. {"features": ["tools"]}')
    unknown = sorted(set(data) - set(KEYS))
    if unknown:
        raise BottleError(f"unknown {'key' if len(unknown) == 1 else 'keys'} {', '.join(unknown)}; a spec has {' and '.join(KEYS)}")
    specs = data.get("features", [])
    if not isinstance(specs, list) or not all(isinstance(spec, str) for spec in specs):
        raise BottleError('features is a list of feature specs, e.g. ["tools", "jvm:version=25"]')
    memory = data.get("memory", repos.ALL_MEMORY)
    if not isinstance(memory, str):
        raise BottleError(f'memory is a string: a size in M or G, e.g. "8G", or "{repos.ALL_MEMORY}"')
    canonical_features = sorted(repos.canonical_features(specs))
    features.resolve(canonical_features)  # every dependency exists, and none is cyclic
    return {"features": canonical_features, "memory": repos.canonical_memory(memory)}


def memory_limit(spec: dict) -> str | None:
    """The spec's memory as a bottle keeps it: a size, or None for all of the machine's."""
    return None if spec["memory"] == repos.ALL_MEMORY else spec["memory"]
