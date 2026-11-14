"""Features: installable additions to an image, in the Dev Container feature format, as folio-cli has them.

folio-cli reads, checks and resolves features (`folio-cli features`; its features.rs says what the format
supports, and what each of customizations.bottle's parts means), and works out what builds their image
(images.rs). This module is its front end: what folio-cli says, as the dataclasses the rest of folio reads. What
builds images, with bottle-cli, is still images.py's (ensure_built, remove_stale).

Features are requested as specs: an id, optionally with option values, e.g.
`jvm:version=17` or `name:a=1,b=true`. Unset options take their defaults.
"""

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from folio.bottle import apt as apt_module, cache as cache_module, egress, images, krun
from folio.bottle.errors import BottleError

FEATURES = Path(__file__).resolve().parents[2] / "features"
BASE = images.BASE
# What a bottle gets in place of a credential, where it needs one to believe it's logged in (see egress.py).
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
    forbid: tuple[egress.Route, ...] = ()
    allow: tuple[egress.Route, ...] = ()
    rewrites: tuple[egress.Rewrite, ...] = ()


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
    repositories: tuple[apt_module.Repository, ...] = ()
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
    try:
        done = subprocess.run([krun.folio_binary(), "features", *args], capture_output=True, text=True,
                              env={**os.environ, "FOLIO_FEATURES": str(FEATURES)})
    except FileNotFoundError:
        raise BottleError(krun.RunnerCli("").missing()) from None
    if done.returncode != 0:
        raise BottleError(krun._error(done.stderr) or f"folio-cli features {' '.join(args)} failed")
    return json.loads(done.stdout)


def _credential(c: dict) -> Credential:
    inject = c["inject"]
    return Credential(c["name"], c["description"], Inject(
        hosts=tuple(inject["hosts"]), value=inject["value"], header=inject["header"], standin=inject["standin"],
        forbid=tuple(egress.Route.parse(r) for r in inject["forbid"]),
        allow=tuple(egress.Route.parse(r) for r in inject["allow"]),
        rewrites=tuple(egress.Rewrite(egress.Route.parse(r["route"]), r["header"], r["json"], r["encoding"])
                       for r in inject["rewrite"]),
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
        apt=Apt(tuple(apt_module.Repository(r["uri"], r["suite"], tuple(r["components"]), r["key"])
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
    return _folio_cli("image", *(f.spec for f in features))["tag"] if features else images.tag(images.BASE)


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


def build_cached() -> list[cache_module.Cached]:
    """What a build keeps of what it downloads: every apt repository, Debian's and every feature's."""
    return [_cached(c) for c in _folio_cli("cached", "--build")]


def cached_for(specs: list[str]) -> list[cache_module.Cached]:
    """The hosts a bottle with these features has its downloads kept for."""
    return [_cached(c) for c in _folio_cli("cached", *specs)]


def _cached(c: dict) -> cache_module.Cached:
    return cache_module.Cached(c["host"], tuple(c["exclude"]), tuple(c["include"]), tuple(c["ports"]),
                               c["immutable"])


def ensure_built(specs: list[str], runner) -> str:
    """Build the base image with the features in `specs` on `runner`, and the base image under it, if missing or
    stale there; return its tag."""
    features = resolve(specs)
    tag = image_tag(features)
    built = _ensure_base(runner)
    if features and not images.is_current(tag, inputs_hash(features), runner.cli):
        print(f"Building {tag}{_where(runner)}...", file=sys.stderr)
        images.build_recipe(FEATURES, tag, inputs_hash(features), recipe(features), cli=runner.cli)
        built = True
    if built:
        for name in remove_stale(runner):
            print(f"Removed stale image {name}", file=sys.stderr)
    return tag


def _where(runner) -> str:
    return "" if runner.name == "local" else f" on runner {runner.name}"


def _ensure_base(runner) -> bool:
    """The base image built on `runner` if it's missing or stale there; whether it was."""
    tag = images.tag(images.BASE)
    if images.is_current(tag, base_inputs(), runner.cli):
        return False
    print(f"Building {tag}{_where(runner)}...", file=sys.stderr)
    images.build_recipe(FEATURES, tag, base_inputs(), base_recipe(), cli=runner.cli)
    return True


def remove_stale(runner) -> list[str]:
    """Forget bottle's images on `runner` whose inputs have changed since they were built; delete stages nothing
    there uses."""
    removed = []
    for tag, labels in images.built(runner.cli).items():
        if labels.get(images.INPUTS_LABEL) != _expected_inputs(tag, labels):
            images.forget(tag, runner.cli)
            removed.append(tag)
    images.remove_unused(runner.cli)
    return removed


def _expected_inputs(tag: str, labels: dict[str, str]) -> str | None:
    """The inputs hash an image with this tag and labels should have now; None if it can't be built any more."""
    if not tag.startswith(f"{images.REPOSITORY}/{images.BASE}:"):
        return None
    try:
        if FEATURES_LABEL not in labels:
            return base_inputs() if tag == images.tag(images.BASE) else None
        return inputs_hash(resolve(labels[FEATURES_LABEL].split()))
    except BottleError:
        return None


FEATURES_LABEL = "bottle.features"
