"""Specs: a bottle's environment -- its features and its memory -- as a file.

    {"features": ["claude", "jvm:version=25", "tools"], "memory": "8G"}

Bottle keeps no specs. Whoever creates a bottle owns the file and passes it to
`bottle new --spec`; `bottle spec validate` checks one the same way, and gives
its canonical form: each feature's options sorted and every default left
out, features sorted, and the memory as a size in M or G. Every spec gives its
memory: a bottle has the size it's given, and no more.
A feature's dependencies aren't written in: they're the feature's to declare,
and resolved again whenever a bottle is created.
"""

import json
import re
import subprocess
from pathlib import Path

from bottle.errors import BottleError

KEYS = ("features", "memory")
MEMORY_PATTERN = re.compile(r"([1-9][0-9]*)([MG])", re.IGNORECASE)


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
    from bottle import features  # imports images, which loading a spec doesn't otherwise need

    if not isinstance(data, dict):
        raise BottleError('a spec is a JSON object, e.g. {"features": ["tools"]}')
    unknown = sorted(set(data) - set(KEYS))
    if unknown:
        raise BottleError(f"unknown {'key' if len(unknown) == 1 else 'keys'} {', '.join(unknown)}; a spec has {' and '.join(KEYS)}")
    specs = data.get("features", [])
    if not isinstance(specs, list) or not all(isinstance(spec, str) for spec in specs):
        raise BottleError('features is a list of feature specs, e.g. ["tools", "jvm:version=25"]')
    canonical = sorted(canonical_features(specs))
    features.resolve(canonical)  # every dependency exists, and none is cyclic
    memory = data.get("memory")
    if memory is None:
        raise BottleError('a spec needs its memory, a size in M or G, e.g. {"memory": "8G"}')
    if not isinstance(memory, str):
        raise BottleError('memory is a string: a size in M or G, e.g. "8G"')
    return {"features": canonical, "memory": canonical_memory(memory)}


def canonical_features(specs: list[str]) -> tuple[str, ...]:
    """Validate feature specs and put each in canonical form, keeping their order."""
    from bottle import features  # imports images, which loading a spec doesn't otherwise need

    result: dict[str, str] = {}
    for spec in specs:
        feature_id, settings = features.parse_spec(spec)
        canonical = features.load(feature_id).configured(settings).spec
        if feature_id in result and result[feature_id] != canonical:
            raise BottleError(f"feature {feature_id} given twice with different options")
        result[feature_id] = canonical
    return tuple(result.values())


def canonical_memory(value: str) -> str:
    """Validate a memory setting: a size in M or G (e.g. 8G) no bigger than the machine's."""
    match = MEMORY_PATTERN.fullmatch(value)
    if not match:
        raise BottleError(f"invalid memory {value!r}: give a size in M or G, e.g. 8G")
    amount, unit = int(match[1]), match[2].upper()
    host = host_memory_mib()
    if amount * (1024 if unit == "G" else 1) > host:
        raise BottleError(f"memory {value} is more than this machine has ({host}M)")
    return f"{amount}{unit}"


def host_memory_mib() -> int:
    """This machine's memory, in MiB."""
    memsize = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, check=True).stdout)
    return memsize // (1024 * 1024)


def memory_limit(spec: dict) -> str:
    """The spec's memory as a bottle keeps it."""
    return spec["memory"]
