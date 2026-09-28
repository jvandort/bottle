import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import features
from bottle.errors import BottleError


class RealFeaturesTest(unittest.TestCase):
    def test_every_shipped_feature_is_valid(self) -> None:
        self.assertEqual(features.available(), ["claude", "jvm", "python", "tools"])
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
        base = {"description": "A token", "env": "TOKEN"}
        return {"bottle": {"credentials": {"tok": {**base, **spec}}}}

    def test_credentials(self) -> None:
        self.feature("a", customizations=self.credential(login={"command": ["cli", "login"], "capture": "token: (\\S+)"}))
        [c] = features.load("a").credentials
        self.assertEqual((c.name, c.env, c.login.command), ("tok", "TOKEN", ("cli", "login")))

    def test_credential_without_a_login_flow(self) -> None:
        self.feature("a", customizations=self.credential())
        self.assertIsNone(features.load("a").credentials[0].login)

    def test_other_tools_customizations_fail_loudly(self) -> None:
        self.assert_invalid("customizations: unsupported: vscode", customizations={"vscode": {"extensions": []}})

    def test_unknown_bottle_customizations(self) -> None:
        self.assert_invalid("customizations.bottle: unsupported: ports", customizations={"bottle": {"ports": []}})

    def test_credential_validation(self) -> None:
        for spec, message in (
            ({"description": ""}, "needs a description"),
            ({"env": "1BAD"}, "env must be an environment variable name"),
            ({"file": "/x"}, "unsupported: file"),
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
