import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import features
from bottle.errors import BottleError


class RealFeaturesTest(unittest.TestCase):
    def test_every_shipped_feature_is_valid(self) -> None:
        self.assertEqual(features.available(), ["claude", "jvm", "python", "teamcity", "tools"])
        for feature_id in features.available():
            with self.subTest(feature_id):
                features.load(feature_id)

    def test_claude_options(self) -> None:
        self.assertEqual(
            features.load("claude").option_env(),
            {"PERMISSIONMODE": "bypassPermissions", "THEME": "dark", "TUI": "default"})
        with self.assertRaisesRegex(BottleError, "option permissionMode must be one of"):
            features.resolve(["claude:permissionMode=yolo"])

    def test_claude_renders_without_capturing_the_mouse(self) -> None:
        # Fullscreen would copy a selection to a clipboard a bottle hasn't got.
        self.assertEqual(features.load("claude").option_env()["TUI"], "default")
        [claude] = features.resolve(["claude:tui=fullscreen"])
        self.assertEqual(claude.option_env()["TUI"], "fullscreen")
        with self.assertRaisesRegex(BottleError, "option tui must be one of default, fullscreen"):
            features.resolve(["claude:tui=full"])

    def test_claude_talks_to_the_api_through_the_proxy_and_holds_no_token(self) -> None:
        claude = features.load("claude")
        [credential] = claude.resolved_credentials()
        self.assertEqual(credential.inject.hosts, ("api.anthropic.com",))
        self.assertEqual((credential.inject.header, credential.inject.value), ("Authorization", "Bearer ${credential}"))
        # Plain http to the API, so the proxy can attach the real token, and a
        # stand-in for the one Claude Code needs to believe it's logged in --
        # which bottle sets per bottle, so it isn't in the image a login runs in.
        self.assertEqual(claude.container_env["ANTHROPIC_BASE_URL"], "http://api.anthropic.com")
        self.assertEqual(credential.inject.standin, "CLAUDE_CODE_OAUTH_TOKEN")
        self.assertNotIn("CLAUDE_CODE_OAUTH_TOKEN", claude.container_env)

    def test_teamcity_needs_a_server_and_attaches_its_token_there(self) -> None:
        [teamcity] = features.resolve(["teamcity:server=https://ci.corp.example.com"])
        [credential] = teamcity.resolved_credentials()
        self.assertEqual(credential.inject.hosts, ("ci.corp.example.com",))
        self.assertEqual((credential.inject.header, credential.inject.value), ("Authorization", "Bearer ${credential}"))
        with self.assertRaisesRegex(BottleError, "feature teamcity needs its server option"):
            features.resolve(["teamcity"])

    def test_teamcity_is_read_only_until_asked_otherwise(self) -> None:
        [teamcity] = features.resolve(["teamcity:server=ci.example.com"])
        self.assertEqual(teamcity.option_env()["READONLY"], "true")
        [writable] = features.resolve(["teamcity:server=ci.example.com,readOnly=false"])
        self.assertEqual(writable.option_env()["READONLY"], "false")

    def test_jvm_options(self) -> None:
        self.assertEqual(features.load("jvm").option_env(), {"VERSION": "25", "ADDITIONALVERSIONS": ""})
        [jvm] = features.resolve(["jvm:version=21,additionalVersions=17,11"])
        self.assertEqual(jvm.option_env(), {"VERSION": "21", "ADDITIONALVERSIONS": "17,11"})

    def test_python_options(self) -> None:
        self.assertEqual(features.load("python").option_env(), {"EXTERNALLYMANAGED": "false"})
        [python] = features.resolve(["python:externallyManaged=true"])
        self.assertEqual(python.option_env(), {"EXTERNALLYMANAGED": "true"})
        with self.assertRaisesRegex(BottleError, "option externallyManaged is true or false"):
            features.resolve(["python:externallyManaged=yes"])


class FeatureTestCase(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        patcher = mock.patch.object(features, "FEATURES", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)

    def feature(self, feature_id: str, install: bool = True, **spec) -> None:
        (self.root / feature_id).mkdir()
        spec = {"id": feature_id, "version": "1.0.0", **spec}
        (self.root / feature_id / "devcontainer-feature.json").write_text(json.dumps(spec))
        if install:
            (self.root / feature_id / "install.sh").write_text("#!/bin/sh\n")


class LoadTest(FeatureTestCase):
    def test_minimal(self) -> None:
        self.feature("a")
        a = features.load("a")
        self.assertEqual((a.id, a.version, a.options, a.container_env, a.depends_on), ("a", "1.0.0", {}, {}, ()))

    def test_metadata_is_accepted(self) -> None:
        self.feature("a", name="A", description="d", documentationURL="u", licenseURL="l", keywords=["k"])
        features.load("a")

    def test_options(self) -> None:
        self.feature("a", options={
            "version": {"type": "string", "default": "21", "proposals": ["17", "21"]},
            "with-docs": {"type": "boolean", "default": False, "description": "d"},
        })
        self.assertEqual(features.load("a").option_env(), {"VERSION": "21", "WITH_DOCS": "false"})

    def assert_invalid(self, pattern: str, install: bool = True, **spec) -> None:
        self.feature("a", install=install, **spec)
        with self.assertRaisesRegex(BottleError, pattern):
            features.load("a")

    def test_unsupported_keys_fail_loudly(self) -> None:
        self.assert_invalid(r"a/devcontainer-feature.json: unsupported: mounts, privileged", mounts=[], privileged=True)

    def test_lifecycle_hooks_are_unsupported(self) -> None:
        self.assert_invalid("unsupported: postCreateCommand", postCreateCommand="echo hi")

    def test_id_must_match_directory(self) -> None:
        self.assert_invalid("id must be 'a'", id="b")

    def test_version_is_required(self) -> None:
        self.assert_invalid("version is required", version="")

    def test_install_sh_is_required(self) -> None:
        self.assert_invalid("install.sh is missing", install=False)

    def test_option_types(self) -> None:
        self.assert_invalid("type must be one of string, boolean", options={"x": {"type": "number", "default": 1}})

    def test_option_default_must_match_its_type(self) -> None:
        self.assert_invalid("needs a boolean default", options={"x": {"type": "boolean", "default": "true"}})

    def test_option_default_is_required(self) -> None:
        self.assert_invalid("needs a string default", options={"x": {"type": "string"}})

    def test_option_default_in_enum(self) -> None:
        self.assert_invalid("default isn't in its enum", options={"x": {"type": "string", "default": "c", "enum": ["a", "b"]}})

    def test_unsupported_option_keys(self) -> None:
        self.assert_invalid("option 'x': unsupported: format", options={"x": {"type": "string", "default": "", "format": "uri"}})

    def test_container_env_values_are_strings(self) -> None:
        self.assert_invalid("containerEnv must map names to strings", containerEnv={"X": 1})

    def test_container_env_names(self) -> None:
        self.assert_invalid("invalid variable name '1X'", containerEnv={"1X": "v"})

    def test_remote_dependencies_are_unsupported(self) -> None:
        self.assert_invalid("remote features aren't supported", dependsOn={"ghcr.io/devcontainers/features/node:1": {}})

    def test_dependency_options_are_unsupported(self) -> None:
        self.feature("b")
        self.assert_invalid("options for dependencies aren't supported", dependsOn={"b": {"version": "2"}})

    def test_unknown_dependency(self) -> None:
        self.assert_invalid("dependsOn: no feature named 'ghost'", dependsOn={"ghost": {}})

    def test_unknown_installs_after(self) -> None:
        self.assert_invalid("installsAfter: no feature named 'ghost'", installsAfter=["ghost"])

    def credential(self, **spec) -> dict:
        """customizations for one credential, attached to ci.example.com unless it says otherwise."""
        base = {"description": "A token", "inject": {"hosts": ["ci.example.com"], "value": "Bearer ${credential}"}}
        return {"bottle": {"credentials": {"tok": {**base, **spec}}}}

    def test_credentials(self) -> None:
        self.feature("a", customizations=self.credential(login={"command": ["cli", "login"], "capture": "token: (\\S+)"}))
        [c] = features.load("a").credentials
        self.assertEqual((c.name, c.inject.hosts, c.login.command), ("tok", ("ci.example.com",), ("cli", "login")))

    def test_credential_without_a_login_flow(self) -> None:
        self.feature("a", customizations=self.credential())
        self.assertIsNone(features.load("a").credentials[0].login)

    def test_credential_injected_at_a_hosts_option(self) -> None:
        self.feature(
            "a",
            options={"server": {"type": "string", "default": "ci.example.com"}},
            customizations=self.credential(inject={
                "hosts": ["${server}", "*.mirror.example.com"], "value": "token ${credential}", "header": "X-Auth"}),
        )
        [c] = features.load("a").resolved_credentials()
        self.assertEqual(c.inject.hosts, ("ci.example.com", "*.mirror.example.com"))
        self.assertEqual((c.inject.header, c.inject.value), ("X-Auth", "token ${credential}"))
        [configured] = features.resolve(["a:server=other.example.com"])
        self.assertEqual(configured.resolved_credentials()[0].inject.hosts[0], "other.example.com")

    def test_an_option_holding_a_server_may_be_written_as_a_url(self) -> None:
        self.feature(
            "a",
            options={"server": {"type": "string", "default": ""}},
            customizations=self.credential(inject={"hosts": ["${server}"], "value": "Bearer ${credential}"}),
        )
        [https] = features.resolve(["a:server=https://ci.example.com/"])
        self.assertEqual(https.resolved_credentials()[0].inject.hosts, ("ci.example.com",))
        [plain] = features.resolve(["a:server=http://ci.example.com"])
        with self.assertRaisesRegex(BottleError, "must be a host, reached over https"):
            plain.resolved_credentials()

    def test_a_credential_belongs_to_hosts_and_never_to_the_bottle(self) -> None:
        for spec, message in (
            ({}, "needs inject, the hosts the proxy attaches it to"),
            ({"env": "TOKEN"}, "unsupported: env"),  # a bottle can't be handed one any more
        ):
            with self.subTest(spec):
                self.feature("a", customizations={"bottle": {"credentials": {"tok": {"description": "d", **spec}}}})
                with self.assertRaisesRegex(BottleError, message):
                    features.load("a")
                __import__("shutil").rmtree(self.root / "a")

    def test_required_options(self) -> None:
        self.feature("a", options={"server": {"type": "string", "default": ""}},
                     customizations={"bottle": {"requiredOptions": ["server"]}})
        self.assertEqual(features.load("a").required_options, ("server",))
        with self.assertRaisesRegex(BottleError, "feature a needs its server option: --feature a:server="):
            features.resolve(["a"])
        with self.assertRaisesRegex(BottleError, "its server option can't be empty"):
            features.resolve(["a:server="])
        self.assertEqual(features.resolve(["a:server=ci.example.com"])[0].values, {"server": "ci.example.com"})

    def test_a_required_option_is_what_the_user_set_not_what_it_defaults_to(self) -> None:
        # The placeholder default is never a value: setting the option to the
        # same string is still setting it, and the feature says so.
        self.feature("a", options={"server": {"type": "string", "default": "ci.example.com"}},
                     customizations={"bottle": {"requiredOptions": ["server"]}})
        [configured] = features.resolve(["a:server=ci.example.com"])
        self.assertEqual(configured.spec, "a:server=ci.example.com")
        with self.assertRaisesRegex(BottleError, "needs its server option"):
            features.resolve(["a"])

    def test_required_options_must_be_options(self) -> None:
        self.assert_invalid("requiredOptions: no option 'ghost'", customizations={"bottle": {"requiredOptions": ["ghost"]}})

    def test_credential_injected_at_an_option_nobody_set(self) -> None:
        self.feature(
            "a",
            options={"server": {"type": "string", "default": ""}},
            customizations=self.credential(inject={"hosts": ["${server}"], "value": "Bearer ${credential}"}),
        )
        with self.assertRaisesRegex(BottleError, "isn't set: use --feature a:server="):
            features.load("a").resolved_credentials()

    def test_inject_validation(self) -> None:
        for spec, message in (
            ({"hosts": [], "value": "Bearer ${credential}"}, "inject.hosts must be a non-empty list"),
            ({"hosts": ["${nope}"], "value": "Bearer ${credential}"}, "names no option of this feature"),
            ({"hosts": ["${flag}"], "value": "Bearer ${credential}"}, "option 'flag' is a boolean, not a host"),
            ({"hosts": ["x"], "value": "Bearer"}, r"must be the header value, containing \$\{credential\}"),
            ({"hosts": ["x"], "value": "Bearer ${credential}", "header": "bad header"}, "must be a header name"),
            ({"hosts": ["https://x"], "value": "Bearer ${credential}"}, "always reached over https"),
            ({"hosts": ["x"], "value": "Bearer ${credential}", "scheme": "http"}, "inject: unsupported: scheme"),
        ):
            with self.subTest(message):
                self.feature("a", options={"flag": {"type": "boolean", "default": False}},
                             customizations=self.credential(inject=spec))
                with self.assertRaisesRegex(BottleError, message):
                    features.load("a")
                __import__("shutil").rmtree(self.root / "a")

    def test_other_tools_customizations_fail_loudly(self) -> None:
        self.assert_invalid("customizations: unsupported: vscode", customizations={"vscode": {"extensions": []}})

    def test_unknown_bottle_customizations(self) -> None:
        self.assert_invalid("customizations.bottle: unsupported: ports", customizations={"bottle": {"ports": []}})

    def test_credential_validation(self) -> None:
        for spec, message in (
            ({"description": ""}, "needs a description"),
            ({"file": "/x"}, "unsupported: file"),
            ({"inject": {"hosts": ["x"], "value": "Bearer ${credential}", "standin": "1BAD"}},
             "inject.standin must be an environment variable name"),
            ({"login": {"command": ["x"]}}, "login needs exactly command and capture"),
            ({"login": {"command": [], "capture": "(x)"}}, "non-empty list of strings"),
            ({"login": {"command": ["x"], "capture": "no group"}}, "exactly one group"),
            ({"login": {"command": ["x"], "capture": "(unclosed"}}, "isn't a valid regular expression"),
        ):
            with self.subTest(message):
                self.feature("a", customizations=self.credential(**spec))
                with self.assertRaisesRegex(BottleError, message):
                    features.load("a")
                __import__("shutil").rmtree(self.root / "a")

    def test_invalid_json(self) -> None:
        (self.root / "a").mkdir()
        (self.root / "a" / "devcontainer-feature.json").write_text("{nope")
        with self.assertRaisesRegex(BottleError, "invalid JSON"):
            features.load("a")

    def test_unknown_feature(self) -> None:
        self.feature("a")
        with self.assertRaisesRegex(BottleError, "no feature named 'nope'; available: a"):
            features.load("nope")


class OptionTest(FeatureTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.feature("f", options={
            "version": {"type": "string", "default": "25"},
            "docs": {"type": "boolean", "default": False},
            "flavor": {"type": "string", "default": "a", "enum": ["a", "b"]},
        })

    def one(self, spec: str) -> features.Feature:
        [feature] = features.resolve([spec])
        return feature

    def test_parse(self) -> None:
        self.assertEqual(features.parse_spec("f"), ("f", {}))
        self.assertEqual(features.parse_spec("f:version=17,docs=true"), ("f", {"version": "17", "docs": "true"}))
        self.assertEqual(features.parse_spec("f:list=1,2,3,x=y"), ("f", {"list": "1,2,3", "x": "y"}))
        self.assertEqual(features.parse_spec("f:url=a=b"), ("f", {"url": "a=b"}))

    def test_bad_specs(self) -> None:
        for spec in ("f:", "f:version", "f:=1", "f:,x=1", "f:version=1,version=2"):
            with self.subTest(spec), self.assertRaises(BottleError):
                features.parse_spec(spec)

    def test_values_and_spec(self) -> None:
        f = self.one("f:version=17,docs=true")
        self.assertEqual(f.values, {"version": "17", "docs": True, "flavor": "a"})
        self.assertEqual(f.spec, "f:docs=true,version=17")  # canonical: sorted
        self.assertEqual(f.option_env(), {"VERSION": "17", "DOCS": "true", "FLAVOR": "a"})

    def test_defaults_are_left_out_of_the_spec(self) -> None:
        self.assertEqual(self.one("f:version=25,flavor=a").spec, "f")

    def test_unknown_option(self) -> None:
        with self.assertRaisesRegex(BottleError, "feature f has no option 'nope' \\(options: docs, flavor, version\\)"):
            self.one("f:nope=1")

    def test_boolean_values(self) -> None:
        with self.assertRaisesRegex(BottleError, "option docs is true or false, not 'yes'"):
            self.one("f:docs=yes")

    def test_enum(self) -> None:
        with self.assertRaisesRegex(BottleError, "option flavor must be one of a, b, not 'c'"):
            self.one("f:flavor=c")

    def test_same_feature_with_different_options(self) -> None:
        with self.assertRaisesRegex(BottleError, "feature f requested twice with different options"):
            features.resolve(["f:version=17", "f:version=21"])

    def test_dependencies_take_defaults_unless_requested(self) -> None:
        self.feature("g", dependsOn={"f": {}})
        self.assertEqual([x.spec for x in features.resolve(["g"])], ["f", "g"])
        self.assertEqual([x.spec for x in features.resolve(["g", "f:version=17"])], ["f:version=17", "g"])


class ResolveTest(FeatureTestCase):
    def ids(self, *requested: str) -> list[str]:
        return [f.id for f in features.resolve(list(requested))]

    def test_ties_break_by_id(self) -> None:
        for name in ("c", "a", "b"):
            self.feature(name)
        self.assertEqual(self.ids("c", "a", "b"), ["a", "b", "c"])

    def test_depends_on_pulls_in_and_orders_first(self) -> None:
        self.feature("tools")
        self.feature("jvm", dependsOn={"tools": {}})
        self.assertEqual(self.ids("jvm"), ["tools", "jvm"])

    def test_transitive_dependencies(self) -> None:
        self.feature("c")
        self.feature("b", dependsOn={"c": {}})
        self.feature("a", dependsOn={"b": {}})
        self.assertEqual(self.ids("a"), ["c", "b", "a"])

    def test_installs_after_orders_only_when_present(self) -> None:
        self.feature("z")
        self.feature("a", installsAfter=["z"])
        self.assertEqual(self.ids("a"), ["a"])
        self.assertEqual(self.ids("a", "z"), ["z", "a"])

    def test_cycle(self) -> None:
        (self.root / "a").mkdir()
        (self.root / "b").mkdir()
        for me, other in (("a", "b"), ("b", "a")):
            (self.root / me / "install.sh").write_text("")
            (self.root / me / "devcontainer-feature.json").write_text(
                json.dumps({"id": me, "version": "1", "dependsOn": {other: {}}}))
        with self.assertRaisesRegex(BottleError, "cycle: a, b"):
            features.resolve(["a"])

    def test_duplicates_install_once(self) -> None:
        self.feature("tools")
        self.feature("jvm", dependsOn={"tools": {}})
        self.assertEqual(self.ids("jvm", "tools", "jvm"), ["tools", "jvm"])


class ImageTest(FeatureTestCase):
    def test_tag_is_canonical(self) -> None:
        self.feature("tools")
        self.feature("claude")
        tag = features.image_tag("base", features.resolve(["tools", "claude"]))
        self.assertEqual(tag, "bottle/base:with-claude.tools")
        self.assertEqual(features.image_tag("base", features.resolve(["claude", "tools"])), tag)
        self.assertEqual(features.image_tag("base", []), "bottle/base:latest")

    def test_tag_distinguishes_options(self) -> None:
        self.feature("jvm", options={"version": {"type": "string", "default": "25"}})
        default = features.image_tag("base", features.resolve(["jvm"]))
        explicit_default = features.image_tag("base", features.resolve(["jvm:version=25"]))
        other = features.image_tag("base", features.resolve(["jvm:version=17"]))
        self.assertEqual(default, "bottle/base:with-jvm")
        self.assertEqual(explicit_default, default)  # setting an option to its default changes nothing
        self.assertRegex(other, r"^bottle/base:with-jvm-[0-9a-f]{8}$")

    def test_dockerfile(self) -> None:
        self.feature("tools", containerEnv={"EDITOR": "vim"})
        self.feature("jvm", dependsOn={"tools": {}}, options={"version": {"type": "string", "default": "21"}},
                     containerEnv={"JAVA_HOME": "/opt/jdk", "PATH": "/opt/jdk/bin:${PATH}"})

        text = features.dockerfile("base", features.resolve(["jvm"]))

        self.assertTrue(text.splitlines()[1] == "FROM bottle/base:latest")
        self.assertLess(text.index("COPY tools "), text.index("COPY jvm "))
        self.assertIn("_REMOTE_USER=genie", text)
        self.assertIn("VERSION=21 ./install.sh", text)
        self.assertIn('ENV JAVA_HOME="/opt/jdk" PATH="/opt/jdk/bin:${PATH}"', text)
        self.assertIn("printf '%s\\n' \"PATH=$PATH\" >> /etc/environment", text)
        self.assertIn('LABEL bottle.features="jvm tools"', text)

    def test_option_values_are_quoted(self) -> None:
        self.feature("a", options={"flags": {"type": "string", "default": "a b; rm -rf /"}})
        self.assertIn("FLAGS='a b; rm -rf /'", features.dockerfile("base", features.resolve(["a"])))


class BuildTest(FeatureTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.feature("tools")
        self.feature("jvm", dependsOn={"tools": {}})
        self.built: list[str] = []
        for patcher in (
            mock.patch.object(features.images, "ensure_built", side_effect=lambda i: self.built.append(f"image {i}")),
            mock.patch.object(features, "_build", side_effect=lambda i, fs, tag, no_cache=False: self.built.append(tag)),
            mock.patch.object(features, "remove_stale", side_effect=lambda: self.built.append("remove stale") or []),
            mock.patch("sys.stderr"),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_ensure_built_builds_base_then_the_combination_then_cleans_up(self) -> None:
        with mock.patch.object(features.images, "is_current", return_value=False):
            self.assertEqual(features.ensure_built("base", ["jvm"]), "bottle/base:with-jvm.tools")
        self.assertEqual(self.built, ["image base", "bottle/base:with-jvm.tools", "remove stale"])

    def test_ensure_built_skips_a_current_combination(self) -> None:
        with mock.patch.object(features.images, "is_current", return_value=True):
            features.ensure_built("base", ["jvm"])
        self.assertEqual(self.built, ["image base"])

    def test_feature_changes_make_the_combination_stale(self) -> None:
        [jvm, tools] = features.resolve(["jvm"])[::-1]
        before = features.inputs_hash("base", [tools, jvm])
        (self.root / "jvm" / "install.sh").write_text("#!/bin/sh\necho changed\n")
        self.assertNotEqual(features.inputs_hash("base", [tools, jvm]), before)

    def test_no_features_is_just_the_image(self) -> None:
        self.assertEqual(features.ensure_built("base", []), "bottle/base:latest")
        self.assertEqual(self.built, ["image base"])

    def test_build_always_rebuilds_the_combination(self) -> None:
        with mock.patch.object(features.images, "is_current", return_value=True):
            features.build("base", ["jvm"])
        self.assertEqual(self.built, ["image base", "bottle/base:with-jvm.tools", "remove stale"])


class RemoveStaleTest(FeatureTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.feature("tools")
        [tools] = features.resolve(["tools"])
        self.current = features.inputs_hash("base", [tools])
        self.deleted: list[str] = []

    def run_with(self, images: dict[str, tuple[str, dict]], in_use: set[str]) -> list[str]:
        refs = [features.runtime.ImageRef(name, digest) for name, (digest, _) in images.items()]
        with mock.patch.object(features.runtime, "images", return_value=refs), \
                mock.patch.object(features.runtime, "images_in_use", return_value=in_use), \
                mock.patch.object(features.runtime, "image_labels", side_effect=lambda n: images[n][1]), \
                mock.patch.object(features.runtime, "image_delete", side_effect=self.deleted.append):
            return features.remove_stale()

    def test_deletes_stale_unused_bottle_images_only(self) -> None:
        removed = self.run_with({
            "bottle/base:with-tools": ("sha256:a", {"bottle.features": "tools", "bottle.inputs": self.current}),
            "bottle/base:with-old": ("sha256:b", {"bottle.features": "tools", "bottle.inputs": "outdated"}),
            "bottle/base:with-in-use": ("sha256:c", {"bottle.features": "tools", "bottle.inputs": "outdated"}),
            "bottle/base:with-gone": ("sha256:d", {"bottle.features": "no-such-feature", "bottle.inputs": "x"}),
            "bottle/base:with-unlabeled": ("sha256:e", {"bottle.features": "tools"}),
            "debian:13": ("sha256:f", {}),
        }, in_use={"sha256:c"})
        self.assertEqual(removed, ["bottle/base:with-old", "bottle/base:with-gone", "bottle/base:with-unlabeled"])
        self.assertEqual(self.deleted, removed)


if __name__ == "__main__":
    unittest.main()
