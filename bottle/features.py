"""Features: installable additions to an image, in the Dev Container feature format.

Each feature is a directory, containers/features/<id>/, holding a
devcontainer-feature.json and an install.sh (https://containers.dev/implementors/features/).
bottle composes an image from features with builds that run each feature's
install.sh in turn, the way the Dev Container tooling does, but without it: no
Node, no Docker, local features only. The base feature, the bottle contract,
is in every bottle: installed on Debian on its own, so every bottle shares it,
and a bottle's features on that. It isn't one to ask for.

Only a subset of the spec is supported. Anything else in a definition is an
error, so a feature never silently loses behavior it asked for:

  id, version (required); name, description, documentationURL, licenseURL,
  keywords (metadata); options (string and boolean, with defaults);
  containerEnv; dependsOn (local feature ids, without options);
  installsAfter (local feature ids); customizations.bottle (below).

customizations.bottle declares what a feature needs from bottle itself:
credentials, downloads to cache, what it installs with apt, options a user has
to set, and what to tell the agent.

A credential is a secret bottle keeps on the controller and a bottle's tools need
(see bottle/auth.py). It never enters a bottle: a credential says which hosts
it belongs to, and folio serve attaches it to the bottle's requests to
those hosts (see bottle/egress.py).

  "customizations": {"bottle": {"credentials": {"teamcity": {
      "description": "TeamCity access token",
      "inject": {"hosts": ["${server}"], "header": "Authorization",
                 "value": "Bearer ${credential}"}}}}}

    hosts    Where the credential is sent: `host` or `host:port`, always
             reached over https -- a credential is never put on a request the
             proxy sends in the clear. `${option}` is the value of one of this
             feature's options, so the server a user configures is the server
             the credential is attached to; a host that is only `${option}`
             is each of the option's comma-separated values, for a user with
             several servers; and a host may be a pattern
             (`*.example.com`). A request matching several patterns gets one
             header, from the first credential that claims it, and two
             credentials claiming the same host is an error.
    header   The header it goes in; Authorization by default.
    value    The whole header value, with ${credential} where the real one goes.
    standin  An environment variable a bottle gets a fake token in, for a tool
             that won't work until it thinks it's logged in (see below).
    forbid   Routes never sent to the hosts, with the credential or without:
             `METHOD[|METHOD] /path`, `*` for any method, and in the path `*`
             for part of a segment, `**` for any number of them, `{a,b}` for
             either (see egress.Route). For the requests whose answer is a
             credential (a token minted from this one, a key provisioned, a
             runner registered), or that let something outside the bottle
             act with it later: the proxy would otherwise spend the
             credential on them like any other.
    allow    Routes the credential is attached to, when there are any; a
             request on another that presents the stand-in is refused. None
             may reach a forbidden route: forbid is what allow must never
             grow to cover, checked when the feature loads.
    rewrite  Where else a tool presents the stand-in, by route: a header whose
             whole value is the stand-in, or a top-level field of a JSON body,
             either raw or base64 (`"encoding": "base64"`, unpadded):

               {"route": "POST /scans/publish/*/*/token", "json": "accessKey"}
               {"route": "GET /distribution-broker/**", "header": "X-User-Access-Key",
                "encoding": "base64"}

             The credential goes in that place, in the same encoding, when
             what's there is exactly the stand-in, and nowhere else.

  A tool in the bottle talks to the host over https as it always would: its
  machine routes the connection to folio serve, bottle-cli taking the TLS off with
  a certificate from the machine's CA, which the bottle trusts for its
  credentials' and cached hosts and no others, so folio serve can
  read the request and put the credential in place of the stand-in, on the
  requests that present it (see below). A host is a name, an IP address, or
  `*.` and a name, since the CA's name constraint can say nothing else. Anything else the tool needs in order to believe it's
  configured is the feature's own business, in its install.sh.

  A CLI doesn't know someone is attaching its token for it, and usually
  refuses to make a request until it has one. What it gets is a stand-in, and
  never a secret, and sending it is how a request asks for the credential: in
  the header, as the token (`Bearer <stand-in>`) or as the password of Basic
  credentials. A request without it goes to the host as the bottle sent it.
  The feature either writes one itself (the teamcity feature puts it in the
  CLI's config file, as STANDIN) or names an environment variable in
  inject.standin, and bottle sets that variable, to a value of its own, in
  every bottle with the feature.

properties is optional: a file on the machine where a tool already keeps its
credentials, as Java properties of `host=credential` lines (Gradle's
Develocity keys.properties is one), with `${option}` for an option's value and
`~` for the home directory. folio serve reads it each time a bottle starts, and
attaches to each of inject.hosts the credential on that host's line; a host
the file has no line for gets the one stored by `folio auth set`, if any.
Only the line for a host the feature names ever leaves the file.

cache names hosts whose downloads bottle-cli keeps on the machine, so the next
bottle gets them from there (see bottle/cache.py): as with a credential's
host, the bottle's TLS to them is terminated with its machine's CA. exclude is
fnmatch patterns over a request's path, for files a host changes after
they're published, which are never kept.

  "customizations": {"bottle": {"cache": {
      "hosts": ["files.example.com"], "exclude": ["*/index.json"]}}}

apt is what the feature installs with apt (see bottle/apt.py): packages, from
Debian's repositories and any the feature declares. An image's build installs
every repository its features declare, then all their packages, in one step
before any install.sh, which is left with what's the feature's own to do. A
repository stays installed in the image, its key (a file in the feature's
directory) in /etc/apt/keyrings, so a bottle can install more from it. A
package is a name, with ${option} for an option's value: one holding a
comma-separated list is a package for each value, and none for an empty one.
bottle-cli keeps every repository's packages for builds and bottles alike, so a
feature's cache needn't name its repositories' hosts.

  "customizations": {"bottle": {"apt": {
      "repositories": [{"uri": "https://apt.example.com/stable", "suite": "stable",
                        "components": ["main"], "key": "example.asc"}],
      "packages": ["example-cli", "jq"]}}}

requiredOptions names options a user has to set, for the ones with no sensible
default (the teamcity feature's server). The format has no way to say this --
an option must declare a default, of its own type, so there is no null to mean
"none" -- and bottle doesn't read the placeholder default those options
declare: it refuses the feature unless the user set the option.

  "customizations": {"bottle": {"requiredOptions": ["server"]}}

context is what the agent in a bottle with the feature should know about it,
added to its ~/BOTTLE.md under a heading of its own: Markdown, a sentence or
two, saying what it can now do, not how the feature was installed. `${option}`
is the value of one of the feature's options.

  "customizations": {"bottle": {"context": "The `teamcity` command talks to ${server}."}}

Features are requested as specs: an id, optionally with option values, e.g.
`jvm:version=17` or `name:a=1,b=true`. Unset options take their defaults.
"""

import hashlib
import json
import re
import sys
from dataclasses import dataclass, replace
from pathlib import Path

from bottle import apt as apt_module, cache as cache_module, egress, images
from bottle.errors import BottleError

FEATURES = Path(__file__).resolve().parent.parent / "containers" / "features"
ID_PATTERN = re.compile(r"[a-z0-9][a-z0-9-]*")
ENV_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
FEATURES_LABEL = "bottle.features"
# The feature every bottle has, first and on its own: the bottle contract. Not one to ask for.
BASE = images.BASE
REMOTE_USER = "genie"

METADATA_KEYS = {"name", "description", "documentationURL", "licenseURL", "keywords"}
SUPPORTED_KEYS = {"id", "version", "options", "containerEnv", "dependsOn", "installsAfter", "customizations"} | METADATA_KEYS
BOTTLE_KEYS = {"credentials", "cache", "requiredOptions", "context", "apt"}
CACHE_KEYS = {"hosts", "exclude"}
APT_KEYS = {"repositories", "packages"}
REPOSITORY_KEYS = {"uri", "suite", "components", "key"}
# An apt package's name (Debian policy 5.6.1), and ${option} where an option's value goes.
PACKAGE = re.compile(r"(?:[a-z0-9+.-]|\$\{[A-Za-z0-9_]+\})+(?::[a-z0-9-]+)?")
CREDENTIAL_KEYS = {"description", "inject", "properties"}
INJECT_KEYS = {"hosts", "header", "value", "standin", "forbid", "allow", "rewrite"}
REWRITE_KEYS = {"route", "header", "json", "encoding"}
# What a stand-in variable holds: enough for a tool to believe it's configured,
# and obvious in a `env | grep TOKEN` that the real one is elsewhere.
STANDIN = "the-egress-proxy-holds-the-real-one"
CREDENTIAL_PLACEHOLDER = "${credential}"
OPTION_REFERENCE = re.compile(r"\$\{([^}]*)\}")
HEADER_NAME = re.compile(r"[A-Za-z0-9!#$%&'*+.^_`|~-]+")
OPTION_KEYS = {"type", "default", "description", "proposals", "enum"}
OPTION_TYPES = {"string": str, "boolean": bool}


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
    # A variable holding a fake token in the bottle, for a tool that won't
    # work until it thinks it's logged in.
    standin: str | None = None
    # Routes never sent; the only ones the credential goes on, if any; where else it's presented.
    forbid: tuple[egress.Route, ...] = ()
    allow: tuple[egress.Route, ...] = ()
    rewrites: tuple[egress.Rewrite, ...] = ()


@dataclass(frozen=True)
class Credential:
    name: str
    description: str
    # Where it goes: onto the bottle's requests, by folio serve.
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
    """What a feature installs with apt: from Debian's repositories and `repositories`, `packages`."""

    repositories: tuple[apt_module.Repository, ...] = ()
    packages: tuple[str, ...] = ()  # names, with ${option} where an option's value goes


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
    cache: Cache | None = None
    apt: Apt = Apt()
    required_options: tuple[str, ...] = ()
    # What to tell the agent about it, with ${option} where an option's value goes.
    context: str = ""
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

    def check_required(self) -> None:
        """Fail unless the user set every option the feature says they must.

        Some options have no sensible default (which server?), and the Dev
        Container format gives no way to say so: an option must declare a
        default, of its own type, so there is no null to mean "none". The
        feature names those options in customizations.bottle.requiredOptions
        and declares a default bottle never reads -- what's checked here is
        whether the user set the option, not what the value happens to be.
        """
        given = dict(self.overrides)
        for name in self.required_options:
            if name not in given:
                raise BottleError(f"feature {self.id} needs its {name} option: --feature {self.id}:{name}=...")
            if not _option_value(given[name]):
                raise BottleError(f"feature {self.id}: its {name} option can't be empty")

    def resolved_credentials(self) -> tuple[Credential, ...]:
        """This feature's credentials, with ${option} in inject.hosts and properties replaced by what the options are set to.

        A feature says which hosts its credential belongs to; when that is an
        option (the server a user configured), the value has to be there, or
        the credential would be attached to nothing.
        """
        resolved = []
        for credential in self.credentials:
            hosts = tuple(h for host in credential.inject.hosts for h in self._expand(host, credential))
            properties = credential.properties
            if properties is not None:
                properties = OPTION_REFERENCE.sub(lambda m: _option_value(self.values[m.group(1)]), properties) or None
            resolved.append(replace(credential, inject=replace(credential.inject, hosts=hosts), properties=properties))
        return tuple(resolved)

    def _expand(self, host: str, credential: Credential) -> tuple[str, ...]:
        """One of inject.hosts as the hosts it stands for: those in its option, when it is only `${option}`."""
        whole = OPTION_REFERENCE.fullmatch(host)
        if not whole:
            return (self._substitute(host, credential),)
        values = [v.strip() for v in _option_value(self.values[whole.group(1)]).split(",")]
        return tuple(self._substitute(host, credential, v) for v in values if v) or (self._substitute(host, credential, ""),)

    def _substitute(self, host: str, credential: Credential, value: str | None = None) -> str:
        """One of inject.hosts with its ${option} filled in, as a bare host or host:port.

        An option holding a server is the URL a user has at hand, so https://
        is allowed here and dropped; http:// is not, because the proxy only
        ever attaches a credential to a request it makes over TLS.
        """
        option = next(iter(OPTION_REFERENCE.findall(host)), None)
        filled = value if value is not None else OPTION_REFERENCE.sub(lambda m: _option_value(self.values[m.group(1)]), host)
        where = f"{self.id}: credential {credential.name!r}"
        if option and not OPTION_REFERENCE.sub("", filled):
            raise BottleError(
                f"{where} is attached to the {option} option's host, which isn't set: "
                f"use --feature {self.id}:{option}=..."
            )
        filled = filled.removeprefix("https://").rstrip("/")
        if "//" in filled or "/" in filled or "," in filled or any(c.isspace() for c in filled):
            raise BottleError(f"{where}: {filled!r} must be a host, reached over https, with no path")
        return filled

    def apt_packages(self) -> tuple[str, ...]:
        """The packages it installs, its options filled in: a name with an option holding a comma-separated list is
        a package for each of its values, and none for an empty one (the jvm feature's additional versions)."""
        packages = []
        for package in self.apt.packages:
            names = [package]
            for option in dict.fromkeys(OPTION_REFERENCE.findall(package)):
                values = [v.strip() for v in _option_value(self.values[option]).split(",") if v.strip()]
                names = [name.replace(f"${{{option}}}", v) for name in names for v in values]
            for name in names:
                if not PACKAGE.fullmatch(name) or "$" in name:
                    raise BottleError(f"{self.id}: {name!r}, from its options, isn't an apt package's name")
            packages += names
        return tuple(packages)

    def agent_context(self) -> str:
        """What to tell the agent about this feature, with its options filled in; "" for nothing."""
        return OPTION_REFERENCE.sub(lambda m: _option_value(self.values[m.group(1)]), self.context).strip()

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
            # A required option's default is a placeholder, so what the user
            # set is kept even when it matches: it's the value, not a default.
            if value != option.default or name in self.required_options:
                overrides[name] = value
        return replace(self, overrides=tuple(sorted(overrides.items())))


def available() -> list[str]:
    """The features a bottle can ask for: all but base, which every bottle has."""
    return sorted(p.parent.name for p in FEATURES.glob("*/devcontainer-feature.json") if p.parent.name != BASE)


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

    options = _options(spec.get("options", {}), fail)
    customizations = _customizations(spec.get("customizations", {}), options, fail)
    for repository in customizations["apt"].repositories:
        if not (path / repository.key).is_file():
            raise fail(f"customizations.bottle.apt.repositories: {repository.uri}'s key {repository.key!r} is missing")
    return Feature(
        id=feature_id,
        version=spec["version"],
        path=path,
        options=options,
        container_env=_container_env(spec.get("containerEnv", {}), fail),
        depends_on=_depends_on(spec.get("dependsOn", {}), fail),
        installs_after=_id_list(spec.get("installsAfter", []), "installsAfter", fail),
        **customizations,
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


def _customizations(customizations, options, fail) -> dict:
    """What the feature asks of bottle: its credentials, its cache, the options a user has to set, and its context."""
    if not isinstance(customizations, dict):
        raise fail("customizations must be an object")
    unsupported = sorted(set(customizations) - {"bottle"})
    if unsupported:
        raise fail(f"customizations: unsupported: {', '.join(unsupported)} (only bottle is supported)")
    bottle = customizations.get("bottle", {})
    if not isinstance(bottle, dict):
        raise fail("customizations.bottle must be an object")
    unsupported = sorted(set(bottle) - BOTTLE_KEYS)
    if unsupported:
        raise fail(f"customizations.bottle: unsupported: {', '.join(unsupported)}")
    credentials = bottle.get("credentials", {})
    if not isinstance(credentials, dict):
        raise fail("customizations.bottle.credentials must be an object")
    required = bottle.get("requiredOptions", [])
    if not isinstance(required, list) or not all(isinstance(name, str) for name in required):
        raise fail("customizations.bottle.requiredOptions must be a list of option names")
    for name in required:
        if name not in options:
            known = ", ".join(sorted(options)) or "none"
            raise fail(f"customizations.bottle.requiredOptions: no option {name!r} (options: {known})")
    context = bottle.get("context", "")
    if not isinstance(context, str):
        raise fail("customizations.bottle.context must be a string")
    for name in OPTION_REFERENCE.findall(context):
        if name not in options:
            known = ", ".join(sorted(options)) or "none"
            raise fail(f"customizations.bottle.context: ${{{name}}} names no option of this feature (options: {known})")
    return {
        "credentials": tuple(_credential(name, spec, options, fail) for name, spec in credentials.items()),
        "cache": _cache(bottle["cache"], fail) if "cache" in bottle else None,
        "apt": _apt(bottle["apt"], options, fail) if "apt" in bottle else Apt(),
        "required_options": tuple(required),
        "context": context,
    }


def _credential(name, spec, options, fail) -> Credential:
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
    if "inject" not in spec:
        raise fail(f"{where}: needs inject, the hosts the proxy attaches it to; a bottle never holds a credential")
    properties = spec.get("properties")
    if properties is not None:
        if not isinstance(properties, str) or not properties:
            raise fail(f"{where}: properties must be the path of a properties file")
        for option in OPTION_REFERENCE.findall(properties):
            if option not in options or options[option].type != "string":
                raise fail(f"{where}: properties: {properties!r} names no string option of this feature")
    return Credential(name, spec["description"], _inject(spec["inject"], options, where, fail), properties)


def _inject(inject, options, where, fail) -> Inject:
    if not isinstance(inject, dict):
        raise fail(f"{where}: inject must be an object")
    unsupported = sorted(set(inject) - INJECT_KEYS)
    if unsupported:
        raise fail(f"{where}: inject: unsupported: {', '.join(unsupported)}")
    hosts = inject.get("hosts")
    if not isinstance(hosts, list) or not hosts or not all(isinstance(h, str) and h for h in hosts):
        raise fail(f"{where}: inject.hosts must be a non-empty list of hosts")
    for host in hosts:
        # A credential is only ever put on a request the proxy makes over
        # TLS, so a host says nothing about its scheme: there is one.
        if "//" in host:
            raise fail(f"{where}: inject.hosts: {host!r} is a host or host:port, always reached over https")
        for option in OPTION_REFERENCE.findall(host):
            if option not in options:
                known = ", ".join(sorted(options)) or "none"
                raise fail(f"{where}: inject.hosts: {host!r} names no option of this feature (options: {known})")
            if options[option].type != "string":
                raise fail(f"{where}: inject.hosts: option {option!r} is a {options[option].type}, not a host")
    header = inject.get("header", "Authorization")
    if not isinstance(header, str) or not HEADER_NAME.fullmatch(header):
        raise fail(f"{where}: inject.header must be a header name")
    value = inject.get("value")
    if not isinstance(value, str) or CREDENTIAL_PLACEHOLDER not in value:
        raise fail(f"{where}: inject.value must be the header value, containing {CREDENTIAL_PLACEHOLDER}")
    standin = inject.get("standin")
    if standin is not None and (not isinstance(standin, str) or not ENV_NAME.fullmatch(standin)):
        raise fail(f"{where}: inject.standin must be an environment variable name")
    forbid = _routes(inject.get("forbid", []), f"{where}: inject.forbid", fail)
    allow = _routes(inject.get("allow", []), f"{where}: inject.allow", fail)
    rewrites = inject.get("rewrite", [])
    if not isinstance(rewrites, list):
        raise fail(f"{where}: inject.rewrite must be a list")
    rewrites = tuple(_rewrite(r, f"{where}: inject.rewrite", fail) for r in rewrites)
    # forbid asserts what allow must never reach: a route allowed (a rewrite's
    # is) that a forbidden one could also match is allowed too widely.
    for route in (*allow, *(r.route for r in rewrites)):
        for forbidden in forbid:
            if route.overlaps_forbidden(forbidden):
                raise fail(f"{where}: {route.text!r} is allowed, but reaches {forbidden.text!r}, which is forbidden: "
                           "narrow it")
    return Inject(tuple(hosts), value, header, standin, forbid, allow, rewrites)


def _routes(routes, where, fail) -> tuple[egress.Route, ...]:
    if not isinstance(routes, list) or not all(isinstance(r, str) for r in routes):
        raise fail(f"{where} must be a list of routes, e.g. \"POST /api/auth/token\"")
    try:
        return tuple(egress.Route.parse(r) for r in routes)
    except ValueError as e:
        raise fail(f"{where}: {e}") from None


def _rewrite(rewrite, where, fail) -> egress.Rewrite:
    if not isinstance(rewrite, dict):
        raise fail(f"{where}: each is an object")
    unsupported = sorted(set(rewrite) - REWRITE_KEYS)
    if unsupported:
        raise fail(f"{where}: unsupported: {', '.join(unsupported)}")
    route = _routes([rewrite.get("route")], where, fail)[0]
    header, field = rewrite.get("header", ""), rewrite.get("json", "")
    if not isinstance(header, str) or not isinstance(field, str) or bool(header) == bool(field):
        raise fail(f"{where}: {route.text}: needs a header or a json field, not both")
    if header and not HEADER_NAME.fullmatch(header):
        raise fail(f"{where}: {route.text}: {header!r} isn't a header name")
    encoding = rewrite.get("encoding", egress.RAW)
    if encoding not in (egress.RAW, egress.BASE64):
        raise fail(f"{where}: {route.text}: encoding is {egress.RAW} or {egress.BASE64}")
    return egress.Rewrite(route, header, field, encoding)


def _cache(cache, fail) -> Cache:
    if not isinstance(cache, dict):
        raise fail("customizations.bottle.cache must be an object")
    unsupported = sorted(set(cache) - CACHE_KEYS)
    if unsupported:
        raise fail(f"customizations.bottle.cache: unsupported: {', '.join(unsupported)}")
    hosts = cache.get("hosts")
    if not isinstance(hosts, list) or not hosts or not all(isinstance(h, str) for h in hosts):
        raise fail("customizations.bottle.cache.hosts must be a non-empty list of hosts")
    for host in hosts:
        # Always https, on its own port: a host is only ever a name.
        if not cache_module.HOST.fullmatch(host):
            raise fail(f"customizations.bottle.cache.hosts: {host!r} must be a lowercase host name, or *. and one")
    exclude = cache.get("exclude", [])
    if not isinstance(exclude, list) or not all(isinstance(p, str) and p.startswith(("/", "*")) for p in exclude):
        raise fail("customizations.bottle.cache.exclude must be a list of path patterns, each starting with / or *")
    return Cache(tuple(hosts), tuple(exclude))


def _apt(spec, options, fail) -> Apt:
    where = "customizations.bottle.apt"
    if not isinstance(spec, dict):
        raise fail(f"{where} must be an object")
    unsupported = sorted(set(spec) - APT_KEYS)
    if unsupported:
        raise fail(f"{where}: unsupported: {', '.join(unsupported)}")
    repositories = spec.get("repositories", [])
    if not isinstance(repositories, list):
        raise fail(f"{where}.repositories must be a list")
    parsed = []
    for repository in repositories:
        if not isinstance(repository, dict) or set(repository) != REPOSITORY_KEYS:
            raise fail(f"{where}.repositories: each is an object of {', '.join(sorted(REPOSITORY_KEYS))}")
        uri, suite, components, key = (repository[k] for k in ("uri", "suite", "components", "key"))
        if not isinstance(uri, str) or not re.fullmatch(r"https?://[a-z0-9.-]+(/[A-Za-z0-9._~/-]*)?", uri):
            raise fail(f"{where}.repositories: uri {uri!r} must be an http:// or https:// URL")
        if not isinstance(suite, str) or not re.fullmatch(r"[A-Za-z0-9._/-]+", suite):
            raise fail(f"{where}.repositories: suite {suite!r} must be a suite's name, e.g. stable")
        if not isinstance(components, list) or not components or not all(
                isinstance(c, str) and re.fullmatch(r"[A-Za-z0-9._-]+", c) for c in components):
            raise fail(f"{where}.repositories: components must be a non-empty list of names, e.g. [\"main\"]")
        if not isinstance(key, str) or not re.fullmatch(r"[A-Za-z0-9._-]+", key):
            raise fail(f"{where}.repositories: key {key!r} must be the name of a file in the feature's directory")
        parsed.append(apt_module.Repository(uri, suite, tuple(components), key))
    packages = spec.get("packages", [])
    if not isinstance(packages, list) or not all(isinstance(p, str) and PACKAGE.fullmatch(p) for p in packages):
        raise fail(f"{where}.packages must be a list of package names, with ${{option}} for an option's value")
    for package in packages:
        for name in OPTION_REFERENCE.findall(package):
            if name not in options:
                known = ", ".join(sorted(options)) or "none"
                raise fail(f"{where}.packages: ${{{name}}} names no option of this feature (options: {known})")
    return Apt(tuple(parsed), tuple(packages))


def _container_env(env, fail) -> dict[str, str]:
    if not isinstance(env, dict) or not all(isinstance(v, str) for v in env.values()):
        raise fail("containerEnv must map names to strings")
    for name, value in env.items():
        if not ENV_NAME.fullmatch(name):
            raise fail(f"containerEnv: invalid variable name {name!r}")
        # Set as it is, as a build's env step sets it: there's no ${NAME} to expand.
        if "$" in value:
            raise fail(f"containerEnv: {name}: references to other variables ($) aren't supported")
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
        if feature_id == BASE:
            raise fail(f"{field}: every feature is installed on {BASE} already")
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
        if feature_id == BASE:
            raise BottleError(f"{BASE} is in every bottle, under its features: it isn't one to ask for")
        if feature_id in settings and settings[feature_id] != options:
            raise BottleError(f"feature {feature_id} requested twice with different options")
        settings[feature_id] = options

    loaded: dict[str, Feature] = {}
    pending = list(settings)
    while pending:
        feature_id = pending.pop()
        if feature_id not in loaded:
            loaded[feature_id] = load(feature_id).configured(settings.get(feature_id, {}))
            loaded[feature_id].check_required()
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


def image_tag(features: list[Feature]) -> str:
    """The tag for the base image with features: the same features and options always get the same tag.

    Options can't be spelled in a tag, so a feature with options set appears as
    its id plus a short hash of its spec, e.g. jvm-1a2b3c4d.
    """
    if not features:
        return images.tag(images.BASE)
    parts = sorted(f.id if not f.overrides else f"{f.id}-{hashlib.sha256(f.spec.encode()).hexdigest()[:8]}" for f in features)
    return f"{images.REPOSITORY}/{images.BASE}:with-{'.'.join(parts)}"


def build_cached() -> list[cache_module.Cached]:
    """What a build keeps of what it downloads: every apt repository, Debian's and every feature's, whichever the
    build installs, so every build's policy is the same and so is the caching CA bottle-cli answers its https ones
    with (images.BUILD_CA)."""
    repositories = (r for f in [base(), *(load(i) for i in available())] for r in f.apt.repositories)
    return apt_module.cached((*apt_module.DEBIAN, *repositories))


def base() -> Feature:
    """The base feature: the bottle contract, under every bottle's other features."""
    return load(BASE)


def base_recipe() -> dict:
    """What builds the base image: the base feature on Debian, and what a bottle runs when it starts."""
    return {"pull": images.DEBIAN, "entrypoint": images.ENTRYPOINT, "cmd": images.CMD, "labels": {},
            "steps": _steps([base()])}


def recipe(features: list[Feature]) -> dict:
    """What installs `features`, in order, on top of the base image."""
    return {"from": images.tag(images.BASE), "steps": _steps(features), "features": [f.spec for f in features],
            "labels": {FEATURES_LABEL: " ".join(sorted(f.spec for f in features))}}


def _steps(features: list[Feature]) -> list[dict]:
    """What the features install with apt, together; then each feature's install.sh, run in its own directory, the
    features directory mounted read-only in the build (see images.build_recipe), the way the Dev Container tooling
    runs it; then its containerEnv."""
    steps: list[dict] = []
    # What they install with apt, first and all at once (see apt.py): their repositories, then their packages.
    repositories = [(f"{images.MOUNT}/{f.id}/{r.key}", r) for f in features for r in f.apt.repositories]
    packages = list(dict.fromkeys(p for f in features for p in f.apt_packages()))
    if repositories or packages:
        # BOTTLE_BUILD_CA is filled in by the build (see images.build_recipe).
        steps.append({"run": ["/bin/sh", "-c", apt_module.install_script(repositories, packages)],
                      "env": {**images.STEP_ENV, images.BUILD_CA: ""}})
    for f in features:
        # The spec's variables telling install.sh who the container's user is, plus the options.
        env = {
            **images.STEP_ENV,
            "_REMOTE_USER": REMOTE_USER, "_REMOTE_USER_HOME": f"/home/{REMOTE_USER}",
            "_CONTAINER_USER": REMOTE_USER, "_CONTAINER_USER_HOME": f"/home/{REMOTE_USER}",
            **f.option_env(),
        }
        steps.append({"run": images.interpreter(f.path / "install.sh"), "env": env, "cwd": f"{images.MOUNT}/{f.id}"})
        if f.container_env:
            steps.append({"env": f.container_env})
            # The environment reaches every exec; SSH sessions only see /etc/environment (via PAM).
            mirror = " && ".join(
                f"sed -i '/^{k}=/d' /etc/environment && printf '%s\\n' \"{k}=${k}\" >> /etc/environment"
                for k in f.container_env
            )
            steps.append({"run": ["sh", "-c", mirror]})
    return steps


def base_inputs() -> str:
    """What the base image is built from: Debian's reference, what it runs, and the base feature's directory."""
    digest = hashlib.sha256(json.dumps([images.DEBIAN, images.ENTRYPOINT, images.CMD]).encode())
    digest.update(images.tree_hash(FEATURES / BASE).encode())
    return digest.hexdigest()


def inputs_hash(features: list[Feature]) -> str:
    """What the base image with features is built from: base's inputs, and each feature's directory and options."""
    digest = hashlib.sha256(base_inputs().encode())
    for f in features:
        digest.update(f"\0{f.spec}\0{images.tree_hash(f.path)}".encode())
    return digest.hexdigest()


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
    there uses.

    A stage goes once no tag's image is made of it and no machine there was made
    from it: a machine boots its image's stages on every start. Returns the images forgotten.
    """
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


def _option_env_name(option: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]", "_", option).upper()


def _option_value(value: str | bool) -> str:
    return ("true" if value else "false") if isinstance(value, bool) else value
