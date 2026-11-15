"""Features, as folio's `features` command says them (folio-cli's features.rs and images.rs): for tests of the
feature format, which read folio's JSON as these dataclasses, as folio's Python once did.

Features are requested as specs: an id, optionally with option values, e.g.
`jvm:version=17` or `name:a=1,b=true`. Unset options take their defaults.
"""

import json
import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

from tests.support import FOLIO


class BottleError(Exception):
    """What folio's features command refused, and why."""


@dataclass(frozen=True)
class Repository:
    uri: str
    suite: str = ""
    components: tuple[str, ...] = ()
    key: str = ""


@dataclass(frozen=True)
class Cached:
    host: str
    exclude: tuple[str, ...] = ()
    include: tuple[str, ...] = ()
    ports: tuple[int, ...] = (443,)
    immutable: bool = False

FEATURES = Path(__file__).resolve().parents[2] / "features"
BASE = "base"
# What a bottle gets in place of a credential, where it needs one to believe it's logged in (folio-cli's
# serve/credentials.rs puts the real one in its place).
STANDIN = "the-egress-proxy-holds-the-real-one"
CREDENTIAL_PLACEHOLDER = "${credential}"
OPTION_REFERENCE = re.compile(r"\$\{([^}]*)\}")


@dataclass(frozen=True)
class Option:
    type: str  # "string" or "boolean"
    default: str | bool
    enum: tuple[str | bool, ...] | None = None


@dataclass(frozen=True)
class Inject:
    """Where a credential belongs, so folio serve can attach it there."""

    hosts: tuple[str, ...]  # `host` or `host:port`; `${option}` is an option's value
    value: str  # the header value, with ${credential} where the real one goes
    header: str = "Authorization"
    standin: str | None = None
    # Routes as written (`POST /api/auth/token`): never sent; the only ones it goes on, if any; and where else
    # it's presented. folio-cli's to read (routes.rs).
    forbid: tuple[str, ...] = ()
    allow: tuple[str, ...] = ()
    rewrites: tuple["Rewrite", ...] = ()


@dataclass(frozen=True)
class Rewrite:
    """Where else, on the requests `route` names, a tool presents the stand-in: in a header, or a JSON field."""

    route: str
    header: str = ""
    json: str = ""
    encoding: str = "raw"


@dataclass(frozen=True)
class Credential:
    name: str
    description: str
    inject: Inject
    # A properties file on the machine, of host=credential lines, read before the Keychain.
    properties: str | None = None


@dataclass(frozen=True)
class Cache:
    """Hosts whose downloads bottle-cli keeps, but for paths matching `exclude`."""

    hosts: tuple[str, ...]
    exclude: tuple[str, ...] = ()


@dataclass(frozen=True)
class Apt:
    repositories: tuple[Repository, ...] = ()
    packages: tuple[str, ...] = ()  # names, with ${option} where an option's value goes


@dataclass(frozen=True)
class Feature:
    """A feature, its options set (overrides), and what folio-cli made of them (derived)."""

    id: str
    version: str
    path: Path
    options: dict[str, Option]
    container_env: dict[str, str]
    depends_on: tuple[str, ...]
    installs_after: tuple[str, ...]
    credentials: tuple[Credential, ...]
    cache: Cache | None
    apt: Apt
    required_options: tuple[str, ...]
    context: str
    overrides: tuple[tuple[str, str | bool], ...]
    spec: str
    derived: dict

    @property
    def values(self) -> dict[str, str | bool]:
        return dict(self.derived["values"])

    def option_env(self) -> dict[str, str]:
        """Options as install.sh sees them, per the spec's naming rule (e.g. version -> VERSION)."""
        return dict(self.derived["option_env"])

    def agent_context(self) -> str:
        """What to tell the agent about this feature, with its options filled in; "" for nothing."""
        return self.derived["agent_context"]

    def resolved_credentials(self) -> tuple[Credential, ...]:
        """Its credentials, with ${option} in inject.hosts and properties replaced by what the options are set to."""
        return tuple(_credential(c) for c in self._resolved("resolved_credentials"))

    def apt_packages(self) -> tuple[str, ...]:
        """The packages it installs, its options filled in."""
        return tuple(self._resolved("apt_packages"))

    def _resolved(self, what: str):
        if "unresolved" in self.derived:
            raise BottleError(self.derived["unresolved"])
        return self.derived[what]

    def configured(self, settings: dict[str, str]) -> "Feature":
        """This feature with options set from a spec's `name=value` strings."""
        given = ",".join(f"{k}={v}" for k, v in settings.items())
        return _feature(_folio_cli("show", f"{self.id}:{given}" if given else self.id))

    def check_required(self) -> None:
        """Fail unless the user set every option the feature says they must."""
        resolve([self.spec])


def _folio_cli(*args: str):
    """folio-cli's features command's JSON, for this folio's features (FEATURES); BottleError, saying why, if it
    fails."""
    done = subprocess.run([str(FOLIO), "features", *args], capture_output=True, text=True,
                          env={**os.environ, "FOLIO_FEATURES": str(FEATURES)})
    if done.returncode != 0:
        said = done.stderr.strip().splitlines()[-1] if done.stderr.strip() else ""
        raise BottleError(said.removeprefix("folio: error: ") or f"folio features {' '.join(args)} failed")
    return json.loads(done.stdout)


def _credential(c: dict) -> Credential:
    inject = c["inject"]
    return Credential(c["name"], c["description"], Inject(
        hosts=tuple(inject["hosts"]), value=inject["value"], header=inject["header"], standin=inject["standin"],
        forbid=tuple(inject["forbid"]), allow=tuple(inject["allow"]),
        rewrites=tuple(Rewrite(r["route"], r["header"], r["json"], r["encoding"]) for r in inject["rewrite"]),
    ), c["properties"])


def _feature(f: dict) -> Feature:
    def option(o: dict) -> Option:
        return Option(o["type"], o["default"], tuple(o["enum"]) if "enum" in o else None)

    return Feature(
        id=f["id"], version=f["version"], path=Path(f["path"]),
        options={name: option(o) for name, o in f["options"].items()},
        container_env=dict(f["container_env"]), depends_on=tuple(f["depends_on"]),
        installs_after=tuple(f["installs_after"]),
        credentials=tuple(_credential(c) for c in f["credentials"]),
        cache=Cache(tuple(f["cache"]["hosts"]), tuple(f["cache"]["exclude"])) if f["cache"] else None,
        apt=Apt(tuple(Repository(r["uri"], r["suite"], tuple(r["components"]), r["key"])
                      for r in f["apt"]["repositories"]), tuple(f["apt"]["packages"])),
        required_options=tuple(f["required_options"]), context=f["context"],
        overrides=tuple(sorted(f["overrides"].items())), spec=f["spec"], derived=f,
    )


def available() -> list[str]:
    """The features a bottle can ask for: all but base, which every bottle has."""
    return _folio_cli("list")


def load(feature_id: str) -> Feature:
    """A feature, as its definition says, its options at their defaults; BottleError if it isn't valid."""
    if ":" in feature_id or "," in feature_id:
        raise BottleError(f"no feature named {feature_id!r}; available: {', '.join(available()) or 'none'}")
    return _feature(_folio_cli("show", feature_id))


def canonical(spec: str) -> str:
    """A spec in its canonical form, its options checked: `jvm`, or `jvm:version=17`."""
    return _folio_cli("canonical", spec)[0]


def resolve(requested: list[str]) -> list[Feature]:
    """The requested features (specs) plus everything they depend on, in install order."""
    return [_feature(f) for f in _folio_cli("resolve", *requested)]


def image_tag(features: list[Feature]) -> str:
    """The tag for the base image with features: the same features and options always get the same tag."""
    return _folio_cli("image", *(f.spec for f in features))["tag"] if features else "localhost/bottle/base:latest"


def base_inputs() -> str:
    """What the base image is built from: Debian's reference, what it runs, and the base feature's directory."""
    return _folio_cli("image")["inputs"]


def inputs_hash(features: list[Feature]) -> str:
    """What the base image with features is built from: base's inputs, and each feature's directory and options."""
    return _folio_cli("image", *(f.spec for f in features))["inputs"] if features else base_inputs()


def base_recipe() -> dict:
    return _folio_cli("image")["recipe"]


def recipe(features: list[Feature]) -> dict:
    """What installs `features`, in order, on top of the base image."""
    return _folio_cli("image", *(f.spec for f in features))["recipe"]


def build_cached() -> list[Cached]:
    """What a build keeps of what it downloads: every apt repository, Debian's and every feature's."""
    return [_cached(c) for c in _folio_cli("cached", "--build")]


def cached_for(specs: list[str]) -> list[Cached]:
    """The hosts a bottle with these features has its downloads kept for."""
    return [_cached(c) for c in _folio_cli("cached", *specs)]


def _cached(c: dict) -> Cached:
    return Cached(c["host"], tuple(c["exclude"]), tuple(c["include"]), tuple(c["ports"]), c["immutable"])


class images:
    """What features' images are built with, as folio's images.rs has it."""

    MOUNT = "/run/bottle-build"
    BUILD_CA = "BOTTLE_BUILD_CA"
