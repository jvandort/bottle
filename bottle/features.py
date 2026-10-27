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

customizations.bottle declares what a feature needs from bottle itself:
credentials, downloads to cache, options a user has to set, and what to tell
the agent.

A credential is a secret bottle keeps on the host and a bottle's tools need
(see bottle/auth.py). It never enters a bottle: a credential says which hosts
it belongs to, and bottled attaches it to the bottle's requests to
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

  A tool in the bottle talks to the host over https as it always would: the
  proxy terminates the tunnel with a certificate from the bottle's egress CA,
  which the bottle trusts for its credentials' hosts and no others (see
  bottle/ca.py), so it can read the request and put the credential in place
  of the stand-in, on the requests that present it (see below). A host is
  a name, an IP address, or `*.` and a name, since the CA's name constraint
  can say nothing else. Anything else the tool needs in order to believe it's
  configured is the feature's own business, in its install.sh.

  A CLI doesn't know someone is attaching its token for it, and usually
  refuses to make a request until it has one. What it gets is a stand-in, and
  never a secret, and sending it is how a request asks for the credential: in
  the header, as the token (`Bearer <stand-in>`) or as the password of Basic
  credentials. A request without it goes to the host as the bottle sent it.
  The feature either writes one itself (the teamcity feature puts it in the
  CLI's config file, as STANDIN) or names an environment variable in
  inject.standin, and bottle sets that variable, to a value of its own, in
  every bottle with the feature. Bottles only: the throwaway bottle that runs
  a login command gets no stand-ins, so a login flow starts from nothing
  rather than from a token that isn't one.

properties is optional: a file on the machine where a tool already keeps its
credentials, as Java properties of `host=credential` lines (Gradle's
Develocity keys.properties is one), with `${option}` for an option's value and
`~` for the home directory. bottled reads it each time a bottle starts, and
attaches to each of inject.hosts the credential on that host's line; a host
the file has no line for gets the one stored by `bottle auth login`, if any.
Only the line for a host the feature names ever leaves the file.

login is optional: without it, `bottle auth login` asks for the value. capture
is a regular expression with one group, matched across lines against the
command's output with terminal escape codes removed; whitespace is removed from
the captured value, since a narrow terminal may wrap it.

cache names hosts whose downloads bottled keeps on the machine, so the next
bottle gets them from there (see bottle/cache.py): as with a credential's
host, the bottle's TLS to them is terminated with its egress CA. exclude is
fnmatch patterns over a request's path, for files a host changes after
they're published, which are never kept.

  "customizations": {"bottle": {"cache": {
      "hosts": ["files.example.com"], "exclude": ["*/index.json"]}}}

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
import shlex
import sys
import tempfile
from dataclasses import dataclass, replace
from pathlib import Path

from bottle import cache as cache_module, egress, images
from bottle.errors import BottleError

FEATURES = Path(__file__).resolve().parent.parent / "containers" / "features"
ID_PATTERN = re.compile(r"[a-z0-9][a-z0-9-]*")
ENV_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
STAGING = "/tmp/bottle-features"
FEATURES_LABEL = "bottle.features"
REMOTE_USER = "genie"

METADATA_KEYS = {"name", "description", "documentationURL", "licenseURL", "keywords"}
SUPPORTED_KEYS = {"id", "version", "options", "containerEnv", "dependsOn", "installsAfter", "customizations"} | METADATA_KEYS
BOTTLE_KEYS = {"credentials", "cache", "requiredOptions", "context"}
CACHE_KEYS = {"hosts", "exclude"}
CREDENTIAL_KEYS = {"description", "login", "inject", "properties"}
LOGIN_KEYS = {"command", "capture"}
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
class Login:
    command: tuple[str, ...]
    capture: str  # regex with one group


@dataclass(frozen=True)
class Inject:
    """Where a credential belongs, so bottled can attach it there."""

    hosts: tuple[str, ...]  # `host` or `host:port`; `${option}` is an option's value
    value: str  # the header value, with ${credential} where the real one goes
    header: str = "Authorization"
    # A variable holding a fake token in the bottle, for a tool that won't
    # work until it thinks it's logged in. Never set in a login throwaway.
    standin: str | None = None
    # Routes never sent; the only ones the credential goes on, if any; where else it's presented.
    forbid: tuple[egress.Route, ...] = ()
    allow: tuple[egress.Route, ...] = ()
    rewrites: tuple[egress.Rewrite, ...] = ()


@dataclass(frozen=True)
class Credential:
    name: str
    description: str
    # Where it goes: onto the bottle's requests, by bottled.
    inject: Inject
    login: Login | None = None
    # A properties file on the machine, of host=credential lines, read before the Keychain.
    properties: str | None = None


@dataclass(frozen=True)
class Cache:
    """Hosts whose downloads bottled keeps, but for paths matching `exclude`."""

    hosts: tuple[str, ...]
    exclude: tuple[str, ...] = ()


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

    options = _options(spec.get("options", {}), fail)
    return Feature(
        id=feature_id,
        version=spec["version"],
        path=path,
        options=options,
        container_env=_container_env(spec.get("containerEnv", {}), fail),
        depends_on=_depends_on(spec.get("dependsOn", {}), fail),
        installs_after=_id_list(spec.get("installsAfter", []), "installsAfter", fail),
        **_customizations(spec.get("customizations", {}), options, fail),
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
    properties = spec.get("properties")
    if properties is not None:
        if not isinstance(properties, str) or not properties:
            raise fail(f"{where}: properties must be the path of a properties file")
        for option in OPTION_REFERENCE.findall(properties):
            if option not in options or options[option].type != "string":
                raise fail(f"{where}: properties: {properties!r} names no string option of this feature")
    return Credential(name, spec["description"], _inject(spec["inject"], options, where, fail), login, properties)


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


def image_tag(image: str, features: list[Feature]) -> str:
    """The tag for `image` with features: the same features and options always get the same tag.

    Options can't be spelled in a tag, so a feature with options set appears as
    its id plus a short hash of its spec, e.g. jvm-1a2b3c4d.
    """
    if not features:
        return images.tag(image)
    parts = sorted(f.id if not f.overrides else f"{f.id}-{hashlib.sha256(f.spec.encode()).hexdigest()[:8]}" for f in features)
    return f"{images.REPOSITORY}/{image}:with-{'.'.join(parts)}"


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
    built = images.ensure_built(image)
    if features and not images.is_current(tag, inputs_hash(image, features)):
        print(f"Building {tag}...", file=sys.stderr)
        _build(image, features, tag)
        built = True
    if built:
        _report_removed(remove_stale())
    return tag


def remove_stale() -> list[str]:
    """Forget bottle's images whose inputs have changed since they were built; delete archives nothing uses.

    An archive goes once no tag points to it and no bottle was made from it:
    smolvm reads a machine's archive again on every start. Returns the images forgotten.
    """
    from bottle import bottles  # bottles imports this module

    removed = []
    for tag, labels in images.built().items():
        if labels.get(images.INPUTS_LABEL) != _expected_inputs(tag, labels):
            images.forget(tag)
            removed.append(tag)
    images.remove_unused({b.archive for b in bottles.load().values() if b.archive})
    return removed


def _expected_inputs(tag: str, labels: dict[str, str]) -> str | None:
    """The inputs hash an image with this tag and labels should have now; None if it can't be built any more."""
    repository, _, _ = tag.partition(":")
    image = repository.removeprefix(f"{images.REPOSITORY}/")
    try:
        if FEATURES_LABEL not in labels:
            return images.inputs_hash(image)
        return inputs_hash(image, resolve(labels[FEATURES_LABEL].split()))
    except BottleError:
        return None


def build(image: str, specs: list[str], no_cache: bool = False) -> str:
    """Build `image` with the features in `specs`, first building anything missing underneath."""
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
        images.build_context(FEATURES, tag, inputs_hash(image, features), no_cache, dockerfile=generated,
                             labels={FEATURES_LABEL: " ".join(sorted(f.spec for f in features))})


def _option_env_name(option: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]", "_", option).upper()


def _option_value(value: str | bool) -> str:
    return ("true" if value else "false") if isinstance(value, bool) else value
