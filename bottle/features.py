"""Features: installable additions to an image, in the Dev Container feature format.

Each feature is a directory, containers/features/<id>/, holding a
devcontainer-feature.json and an install.sh (https://containers.dev/implementors/features/).
bottle composes an image from a base image plus features by generating a
Dockerfile that runs each feature's install.sh in turn, the way the Dev
Container tooling does, but without it: no Node, no Docker, local features only.

Only a subset of the spec is supported. Anything else in a definition is an
error, so a feature never silently loses behavior it asked for:

  id, version (required); name, description, documentationURL, licenseURL,
  keywords (metadata); options (string and boolean, with defaults);
  containerEnv; dependsOn (local feature ids, without options);
  installsAfter (local feature ids); customizations.bottle (below).

customizations.bottle.credentials declares credentials the feature needs, e.g.
a login token. bottle stores them on the host and delivers them to bottles
with the feature (see bottle/auth.py):

  "customizations": {"bottle": {"credentials": {"claude": {
      "description": "Claude subscription token",
      "login": {"command": ["claude", "setup-token"], "capture": "Your OAuth token[^:]*:(.*?)Store this token"},
      "env": "CLAUDE_CODE_OAUTH_TOKEN"}}}}

login is optional: without it, `bottle auth login` asks for the value. capture
is a regular expression with one group, matched (across lines) against the
command's output with terminal escape codes removed; whitespace is removed from
the captured value, since a narrow terminal may wrap it.

Features are requested as specs: an id, optionally with option values, e.g.
`jvm:version=17` or `name:a=1,b=true`. Unset options take their defaults.
"""

import hashlib
import json
import re
import shlex
import sys
import tempfile
from dataclasses import dataclass, replace
from pathlib import Path

from bottle import images, runtime
from bottle.errors import BottleError

FEATURES = Path(__file__).resolve().parent.parent / "containers" / "features"
ID_PATTERN = re.compile(r"[a-z0-9][a-z0-9-]*")
ENV_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
STAGING = "/tmp/bottle-features"
FEATURES_LABEL = "bottle.features"
REMOTE_USER = "genie"

METADATA_KEYS = {"name", "description", "documentationURL", "licenseURL", "keywords"}
SUPPORTED_KEYS = {"id", "version", "options", "containerEnv", "dependsOn", "installsAfter", "customizations"} | METADATA_KEYS
CREDENTIAL_KEYS = {"description", "login", "env"}
LOGIN_KEYS = {"command", "capture"}
OPTION_KEYS = {"type", "default", "description", "proposals", "enum"}
OPTION_TYPES = {"string": str, "boolean": bool}


@dataclass(frozen=True)
class Option:
    type: str  # "string" or "boolean"
    default: str | bool
    enum: tuple[str | bool, ...] | None = None


@dataclass(frozen=True)
class Login:
    command: tuple[str, ...]
    capture: str  # regex with one group


@dataclass(frozen=True)
class Credential:
    name: str
    description: str
    env: str  # delivered as this environment variable
    login: Login | None = None


@dataclass(frozen=True)
class Feature:
    id: str
    version: str
    path: Path
    options: dict[str, Option]
    container_env: dict[str, str]
    depends_on: tuple[str, ...]
    installs_after: tuple[str, ...]
    credentials: tuple[Credential, ...] = ()
    # Option values set explicitly (not defaulted), from the feature's spec.
    overrides: tuple[tuple[str, str | bool], ...] = ()

    @property
    def values(self) -> dict[str, str | bool]:
        return {name: option.default for name, option in self.options.items()} | dict(self.overrides)

    @property
    def spec(self) -> str:
        """The canonical spec: `jvm`, or `jvm:version=17` when options are set."""
        if not self.overrides:
            return self.id
        return f"{self.id}:" + ",".join(f"{k}={_option_value(v)}" for k, v in self.overrides)

    def option_env(self) -> dict[str, str]:
        """Options as install.sh sees them, per the spec's naming rule (e.g. version -> VERSION)."""
        return {_option_env_name(k): _option_value(v) for k, v in self.values.items()}

    def configured(self, settings: dict[str, str]) -> "Feature":
        """This feature with options set from a spec's `name=value` strings."""
        overrides = {}
        for name, raw in settings.items():
            option = self.options.get(name)
            if option is None:
                known = ", ".join(sorted(self.options)) or "none"
                raise BottleError(f"feature {self.id} has no option {name!r} (options: {known})")
            if option.type == "boolean":
                if raw not in ("true", "false"):
                    raise BottleError(f"{self.id}: option {name} is true or false, not {raw!r}")
                value: str | bool = raw == "true"
            else:
                value = raw
            if option.enum is not None and value not in option.enum:
                allowed = ", ".join(_option_value(v) for v in option.enum)
                raise BottleError(f"{self.id}: option {name} must be one of {allowed}, not {raw!r}")
            if value != option.default:
                overrides[name] = value
        return replace(self, overrides=tuple(sorted(overrides.items())))


def available() -> list[str]:
    return sorted(p.parent.name for p in FEATURES.glob("*/devcontainer-feature.json"))


def load(feature_id: str) -> Feature:
    path = FEATURES / feature_id
    if not ID_PATTERN.fullmatch(feature_id) or not (path / "devcontainer-feature.json").is_file():
        raise BottleError(f"no feature named {feature_id!r}; available: {', '.join(available()) or 'none'}")
    where = f"containers/features/{feature_id}/devcontainer-feature.json"
    try:
        spec = json.loads((path / "devcontainer-feature.json").read_text())
    except json.JSONDecodeError as e:
        raise BottleError(f"{where}: invalid JSON: {e}") from None

    def fail(message: str) -> BottleError:
        return BottleError(f"{where}: {message}")

    if not isinstance(spec, dict):
        raise fail("must be a JSON object")
    unsupported = sorted(set(spec) - SUPPORTED_KEYS)
    if unsupported:
        raise fail(f"unsupported: {', '.join(unsupported)} (bottle supports: {', '.join(sorted(SUPPORTED_KEYS))})")
    if spec.get("id") != feature_id:
        raise fail(f"id must be {feature_id!r}, its directory name")
    if not isinstance(spec.get("version"), str) or not spec["version"]:
        raise fail("version is required")
    if not (path / "install.sh").is_file():
        raise fail("install.sh is missing")

    return Feature(
        id=feature_id,
        version=spec["version"],
        path=path,
        options=_options(spec.get("options", {}), fail),
        container_env=_container_env(spec.get("containerEnv", {}), fail),
        depends_on=_depends_on(spec.get("dependsOn", {}), fail),
        installs_after=_id_list(spec.get("installsAfter", []), "installsAfter", fail),
        credentials=_customizations(spec.get("customizations", {}), fail),
    )


def _options(options, fail) -> dict[str, Option]:
    if not isinstance(options, dict):
        raise fail("options must be an object")
    parsed = {}
    for name, option in options.items():
        if not isinstance(option, dict):
            raise fail(f"option {name!r} must be an object")
        unsupported = sorted(set(option) - OPTION_KEYS)
        if unsupported:
            raise fail(f"option {name!r}: unsupported: {', '.join(unsupported)}")
        kind = OPTION_TYPES.get(option.get("type"))
        if kind is None:
            raise fail(f"option {name!r}: type must be one of {', '.join(OPTION_TYPES)}")
        if "default" not in option or type(option["default"]) is not kind:
            raise fail(f"option {name!r}: needs a {option['type']} default")
        if "enum" in option and option["default"] not in option["enum"]:
            raise fail(f"option {name!r}: default isn't in its enum")
        if not ENV_NAME.fullmatch(_option_env_name(name)):
            raise fail(f"option {name!r}: doesn't make a valid environment variable name")
        enum = option.get("enum")
        parsed[name] = Option(option["type"], option["default"], tuple(enum) if enum is not None else None)
    return parsed


def _customizations(customizations, fail) -> tuple[Credential, ...]:
    if not isinstance(customizations, dict):
        raise fail("customizations must be an object")
    unsupported = sorted(set(customizations) - {"bottle"})
    if unsupported:
        raise fail(f"customizations: unsupported: {', '.join(unsupported)} (only bottle is supported)")
    bottle = customizations.get("bottle", {})
    if not isinstance(bottle, dict):
        raise fail("customizations.bottle must be an object")
    unsupported = sorted(set(bottle) - {"credentials"})
    if unsupported:
        raise fail(f"customizations.bottle: unsupported: {', '.join(unsupported)}")
    credentials = bottle.get("credentials", {})
    if not isinstance(credentials, dict):
        raise fail("customizations.bottle.credentials must be an object")
    return tuple(_credential(name, spec, fail) for name, spec in credentials.items())


def _credential(name, spec, fail) -> Credential:
    where = f"credential {name!r}"
    if not ID_PATTERN.fullmatch(name):
        raise fail(f"{where}: names use lowercase letters, digits and '-'")
    if not isinstance(spec, dict):
        raise fail(f"{where} must be an object")
    unsupported = sorted(set(spec) - CREDENTIAL_KEYS)
    if unsupported:
        raise fail(f"{where}: unsupported: {', '.join(unsupported)}")
    if not isinstance(spec.get("description"), str) or not spec["description"]:
        raise fail(f"{where}: needs a description")
    if not isinstance(spec.get("env"), str) or not ENV_NAME.fullmatch(spec["env"]):
        raise fail(f"{where}: env must be an environment variable name")
    login = None
    if "login" in spec:
        raw = spec["login"]
        if not isinstance(raw, dict) or set(raw) != LOGIN_KEYS:
            raise fail(f"{where}: login needs exactly command and capture")
        command = raw["command"]
        if not isinstance(command, list) or not command or not all(isinstance(c, str) and c for c in command):
            raise fail(f"{where}: login command must be a non-empty list of strings")
        try:
            groups = re.compile(raw["capture"]).groups if isinstance(raw["capture"], str) else -1
        except re.error as e:
            raise fail(f"{where}: login capture isn't a valid regular expression: {e}") from None
        if groups != 1:
            raise fail(f"{where}: login capture must be a regular expression with exactly one group")
        login = Login(tuple(command), raw["capture"])
    return Credential(name, spec["description"], spec["env"], login)


def _container_env(env, fail) -> dict[str, str]:
    if not isinstance(env, dict) or not all(isinstance(v, str) for v in env.values()):
        raise fail("containerEnv must map names to strings")
    for name in env:
        if not ENV_NAME.fullmatch(name):
            raise fail(f"containerEnv: invalid variable name {name!r}")
    return dict(env)


def _depends_on(depends_on, fail) -> tuple[str, ...]:
    if not isinstance(depends_on, dict):
        raise fail("dependsOn must be an object")
    for dependency, options in depends_on.items():
        if options != {}:
            raise fail(f"dependsOn {dependency!r}: options for dependencies aren't supported; use {{}}")
    return _id_list(list(depends_on), "dependsOn", fail)


def _id_list(ids, field, fail) -> tuple[str, ...]:
    if not isinstance(ids, list) or not all(isinstance(i, str) for i in ids):
        raise fail(f"{field} must be a list of feature ids")
    for feature_id in ids:
        if not ID_PATTERN.fullmatch(feature_id):
            raise fail(f"{field}: {feature_id!r} isn't a local feature id (remote features aren't supported)")
        if not (FEATURES / feature_id / "devcontainer-feature.json").is_file():
            raise fail(f"{field}: no feature named {feature_id!r}")
    return tuple(ids)


def parse_spec(spec: str) -> tuple[str, dict[str, str]]:
    """`jvm:version=25,additionalVersions=17,21` -> ("jvm", {"version": "25", "additionalVersions": "17,21"}).

    Options are comma-separated NAME=VALUE pairs. A segment without `=`
    continues the previous value, so list values keep the Dev Container
    convention of commas (as in the Java feature's additionalVersions).
    """
    feature_id, sep, rest = spec.partition(":")
    settings: dict[str, str] = {}
    if sep:
        name = None
        for item in rest.split(","):
            key, eq, value = item.partition("=")
            if eq and key:
                if key in settings:
                    raise BottleError(f"bad feature spec {spec!r}: option {key!r} given twice")
                name, settings[key] = key, value
            elif name is not None and not eq:
                settings[name] += f",{item}"
            else:
                raise BottleError(f"bad feature spec {spec!r}: expected FEATURE:OPTION=VALUE[,OPTION=VALUE]")
    return feature_id, settings


def resolve(requested: list[str]) -> list[Feature]:
    """The requested features (specs) plus everything they depend on, in install order.

    Dependencies take their option defaults unless requested themselves.
    dependsOn and installsAfter both order features; dependsOn also pulls the
    dependency in. Ties are broken by id, so the same set always installs in
    the same order.
    """
    settings: dict[str, dict[str, str]] = {}
    for spec in requested:
        feature_id, options = parse_spec(spec)
        if feature_id in settings and settings[feature_id] != options:
            raise BottleError(f"feature {feature_id} requested twice with different options")
        settings[feature_id] = options

    loaded: dict[str, Feature] = {}
    pending = list(settings)
    while pending:
        feature_id = pending.pop()
        if feature_id not in loaded:
            loaded[feature_id] = load(feature_id).configured(settings.get(feature_id, {}))
            pending.extend(loaded[feature_id].depends_on)

    before = {f.id: {d for d in (*f.depends_on, *f.installs_after) if d in loaded} for f in loaded.values()}
    order: list[Feature] = []
    while before:
        ready = sorted(f for f, deps in before.items() if not deps)
        if not ready:
            raise BottleError(f"features depend on each other in a cycle: {', '.join(sorted(before))}")
        order.append(loaded[ready[0]])
        del before[ready[0]]
        for deps in before.values():
            deps.discard(ready[0])
    return order


def image_tag(image: str, features: list[Feature]) -> str:
    """The tag for `image` with features: the same features and options always get the same tag.

    Options can't be spelled in a tag, so a feature with options set appears as
    its id plus a short hash of its spec, e.g. jvm-1a2b3c4d.
    """
    if not features:
        return images.tag(image)
    parts = sorted(f.id if not f.overrides else f"{f.id}-{hashlib.sha256(f.spec.encode()).hexdigest()[:8]}" for f in features)
    return f"bottle/{image}:with-{'.'.join(parts)}"


def dockerfile(image: str, features: list[Feature]) -> str:
    """A Dockerfile that installs `features`, in order, on top of `image`."""
    lines = [
        f"# Generated by bottle: {image} + {', '.join(f.id for f in features)}",
        f"FROM {images.tag(image)}",
        "ARG DEBIAN_FRONTEND=noninteractive",
    ]
    for f in features:
        # The spec's variables telling install.sh who the container's user is, plus the options.
        env = {
            "_REMOTE_USER": REMOTE_USER, "_REMOTE_USER_HOME": f"/home/{REMOTE_USER}",
            "_CONTAINER_USER": REMOTE_USER, "_CONTAINER_USER_HOME": f"/home/{REMOTE_USER}",
            **f.option_env(),
        }
        assignments = " ".join(f"{k}={shlex.quote(v)}" for k, v in env.items())
        lines += [
            "",
            f"# {f.id} {f.version}",
            f"COPY {f.id} {STAGING}/{f.id}",
            f"RUN cd {STAGING}/{f.id} && chmod +x install.sh && env {assignments} ./install.sh && rm -rf {STAGING}/{f.id}",
        ]
        if f.container_env:
            lines.append("ENV " + " ".join(f"{k}={json.dumps(v)}" for k, v in f.container_env.items()))
            # ENV reaches `container exec`; SSH sessions only see /etc/environment (via PAM).
            mirror = " && ".join(
                f"sed -i '/^{k}=/d' /etc/environment && printf '%s\\n' \"{k}=${k}\" >> /etc/environment"
                for k in f.container_env
            )
            lines.append(f"RUN {mirror}")
    lines += ["", f"LABEL {FEATURES_LABEL}={json.dumps(' '.join(sorted(f.spec for f in features)))}", ""]
    return "\n".join(lines)


def inputs_hash(image: str, features: list[Feature]) -> str:
    """What an image with features is built from: the image's inputs, and each feature's directory and options."""
    digest = hashlib.sha256(images.inputs_hash(image).encode())
    for f in features:
        digest.update(f"\0{f.spec}\0{images.tree_hash(f.path)}".encode())
    return digest.hexdigest()


def ensure_built(image: str, specs: list[str]) -> str:
    """Build `image` with the features in `specs` if missing or stale; return its tag."""
    features = resolve(specs)
    tag = image_tag(image, features)
    with images.building():
        built = images.ensure_built(image)
        if features and not images.is_current(tag, inputs_hash(image, features)):
            print(f"Building {tag}...", file=sys.stderr)
            _build(image, features, tag)
            built = True
    if built:
        _report_removed(remove_stale())
    return tag


def remove_stale() -> list[str]:
    """Delete bottle's images whose inputs have changed since they were built, unless a container uses one.

    Returns the images deleted.
    """
    in_use = runtime.images_in_use()
    removed = []
    for ref in runtime.images():
        if not ref.name.startswith("bottle/") or ref.digest in in_use:
            continue
        labels = runtime.image_labels(ref.name) or {}
        if labels.get(images.INPUTS_LABEL) != _expected_inputs(ref.name, labels):
            runtime.image_delete(ref.name)
            removed.append(ref.name)
    return removed


def _expected_inputs(tag: str, labels: dict[str, str]) -> str | None:
    """The inputs hash an image with this tag and labels should have now; None if it can't be built any more."""
    repository, _, _ = tag.partition(":")
    image = repository.removeprefix("bottle/")
    try:
        if FEATURES_LABEL not in labels:
            return images.inputs_hash(image)
        return inputs_hash(image, resolve(labels[FEATURES_LABEL].split()))
    except BottleError:
        return None


def build(image: str, specs: list[str], no_cache: bool = False) -> str:
    """Build `image` with the features in `specs`, first building anything missing underneath."""
    with images.building():
        if not specs:
            tag = images.build(image, no_cache)
        else:
            features = resolve(specs)
            tag = image_tag(image, features)
            images.ensure_built(image)
            _build(image, features, tag, no_cache)
    _report_removed(remove_stale())
    return tag


def _report_removed(removed: list[str]) -> None:
    for name in removed:
        print(f"Removed stale image {name}", file=sys.stderr)


def _build(image: str, features: list[Feature], tag: str, no_cache: bool = False) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        generated = Path(tmp) / "Dockerfile"
        generated.write_text(dockerfile(image, features))
        images.build_context(FEATURES, tag, inputs_hash(image, features), no_cache, dockerfile=generated)


def _option_env_name(option: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]", "_", option).upper()


def _option_value(value: str | bool) -> str:
    return ("true" if value else "false") if isinstance(value, bool) else value
