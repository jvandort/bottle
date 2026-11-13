"""Specs: a bottle's environment -- its features and its memory -- as a file.

    {"features": ["claude", "jvm:version=25", "tools"], "memory": "8G"}

A spec can also give the bottle permissions, what folio serve lets it do beyond
what every bottle may (see PERMISSIONS), and labels: its creator's names for
it, which bottle doesn't read (folio's say which thread it's for), shown with
its machine (`bottle-cli ls`) and to bottles with list-bottles:

    {"features": ["claude"], "memory": "4G", "permissions": ["message-any"], "labels": {"thread": "fix-it"}}

Whoever creates a bottle owns its spec (folio: a repo's, a sidecar's), and
canonical checks it and gives its canonical form: each feature's options
sorted and every default left out, features sorted, and the memory as a size
in M or G. Every spec gives its memory: a bottle has the size it's given, and
no more.
A feature's dependencies aren't written in: they're the feature's to declare,
and resolved again whenever a bottle is created.
"""

import re
import platform
import subprocess

from folio.bottle.errors import BottleError

KEYS = ("features", "memory", "disk", "permissions", "labels")
# What a bottle may be allowed beyond answering messages (see messages.py), and
# what its agent is told it can do with each (bottles.context).
PERMISSIONS = {
    "list-bottles": "You can list the other running bottles: `GET http://bottle.host/bottles` "
                    "gives each one's name, repo and labels (its creator's: a folio thread's says which thread).",
    "message-any": "You can send a message to any running bottle by its name, not only answer one: "
                   'its session gets it as an event from you. `POST http://bottle.host/messages` '
                   'with `{"to": NAME, "text": ...}`.',
}
# A label's name, and its value: anything printable, as long as a line.
LABEL_PATTERN = re.compile(r"[a-z][a-z0-9.-]*")
LABEL_VALUE_MAX = 200
MEMORY_PATTERN = re.compile(r"([1-9][0-9]*)([MG])", re.IGNORECASE)
# The disk a bottle writes to, in whole G: bottle-cli sizes it in GiB.
DISK_PATTERN = re.compile(r"([1-9][0-9]*)G", re.IGNORECASE)


def canonical(data: object, memory_mib: int | None = None, where: str = "this machine") -> dict:
    """Validate a spec and put it in canonical form.

    Given `memory_mib`, the memory of the machine it's for (`where`), its
    memory must fit it too: which machine a spec is for, its bottle knows and a
    spec doesn't, since the same spec makes bottles on any runner.
    """
    from folio.bottle import features  # imports images, which loading a spec doesn't otherwise need

    if not isinstance(data, dict):
        raise BottleError('a spec is a JSON object, e.g. {"features": ["tools"]}')
    unknown = sorted(set(data) - set(KEYS))
    if unknown:
        raise BottleError(f"unknown {'key' if len(unknown) == 1 else 'keys'} {', '.join(unknown)}; "
                          f"a spec has {', '.join(KEYS[:-1])} and {KEYS[-1]}")
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
    spec = {"features": canonical, "memory": canonical_memory(memory, memory_mib, where)}
    disk = data.get("disk")
    if disk is not None:
        if not isinstance(disk, str) or not DISK_PATTERN.fullmatch(disk):
            raise BottleError(f'invalid disk {disk!r}: give a size in G, e.g. "50G"')
        spec["disk"] = disk.upper()
    permissions = data.get("permissions", [])
    if not isinstance(permissions, list) or not all(isinstance(p, str) for p in permissions):
        raise BottleError(f'permissions is a list of permissions, e.g. ["message-any"]; there are {", ".join(PERMISSIONS)}')
    unknown = sorted(set(permissions) - set(PERMISSIONS))
    if unknown:
        raise BottleError(f"unknown {'permission' if len(unknown) == 1 else 'permissions'} {', '.join(unknown)}; "
                          f"there are {', '.join(PERMISSIONS)}")
    if permissions:
        spec["permissions"] = sorted(set(permissions))
    given = data.get("labels", {})
    if not isinstance(given, dict):
        raise BottleError('labels map names to values, e.g. {"thread": "fix-it"}')
    for name, value in given.items():
        if not LABEL_PATTERN.fullmatch(name):
            raise BottleError(f"invalid label {name!r}: lowercase letters, digits, '.' and '-', starting with a letter")
        if not isinstance(value, str) or len(value) > LABEL_VALUE_MAX or not value.isprintable():
            raise BottleError(f"label {name}'s value must be printable text, at most {LABEL_VALUE_MAX} characters")
    if given:
        spec["labels"] = dict(sorted(given.items()))
    return spec


def labels(spec: dict) -> dict[str, str]:
    """A spec's labels; one saved before labels, a thread's, as the label thread."""
    if "labels" in spec:
        return dict(spec["labels"])
    return {"thread": spec["thread"]} if spec.get("thread") else {}


def canonical_features(specs: list[str]) -> tuple[str, ...]:
    """Validate feature specs and put each in canonical form, keeping their order."""
    from folio.bottle import features  # imports images, which loading a spec doesn't otherwise need

    result: dict[str, str] = {}
    for spec in specs:
        feature_id, settings = features.parse_spec(spec)
        canonical = features.load(feature_id).configured(settings).spec
        if feature_id in result and result[feature_id] != canonical:
            raise BottleError(f"feature {feature_id} given twice with different options")
        result[feature_id] = canonical
    return tuple(result.values())


def canonical_memory(value: str, memory_mib: int | None = None, where: str = "this machine") -> str:
    """Validate a memory setting: a size in M or G (e.g. 8G), and no bigger than `memory_mib` if it's given."""
    match = MEMORY_PATTERN.fullmatch(value)
    if not match:
        raise BottleError(f"invalid memory {value!r}: give a size in M or G, e.g. 8G")
    amount, unit = int(match[1]), match[2].upper()
    if memory_mib is not None and amount * (1024 if unit == "G" else 1) > memory_mib:
        raise BottleError(f"memory {value} is more than {where} has ({memory_mib}M)")
    return f"{amount}{unit}"


def host_memory_mib() -> int:
    """This machine's memory, in MiB: a Mac's from sysctl, Linux's from /proc/meminfo."""
    if platform.system() == "Linux":
        with open("/proc/meminfo") as meminfo:
            for line in meminfo:
                if line.startswith("MemTotal:"):
                    return int(line.split()[1]) // 1024
        raise BottleError("/proc/meminfo has no MemTotal")
    memsize = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, check=True).stdout)
    return memsize // (1024 * 1024)


def disk_gib(spec: dict) -> int | None:
    """The spec's disk in GiB; None: bottle-cli's default (64G, sparse)."""
    return int(spec["disk"][:-1]) if "disk" in spec else None


def memory_limit(spec: dict) -> str:
    """The spec's memory as a bottle keeps it."""
    return spec["memory"]
