import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

from bottle import specs
from bottle.errors import BottleError


class CanonicalTest(unittest.TestCase):
    def setUp(self) -> None:
        patcher = mock.patch.object(specs.repos.runtime, "host_memory_mib", return_value=16384)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_features_are_sorted_with_defaults_left_out(self) -> None:
        spec = specs.canonical({"features": ["tools", "jvm:version=25,additionalVersions=17,21", "claude"]})
        self.assertEqual(spec, {"features": ["claude", "jvm:additionalVersions=17,21", "tools"], "memory": "all"})

    def test_memory_as_container_takes_it(self) -> None:
        self.assertEqual(specs.canonical({"memory": "8g"})["memory"], "8G")
        self.assertEqual(specs.canonical({"memory": "all"})["memory"], "all")

    def test_an_empty_spec_is_no_features_and_all_the_memory(self) -> None:
        self.assertEqual(specs.canonical({}), {"features": [], "memory": "all"})

    def test_dependencies_are_left_to_the_feature(self) -> None:
        # Resolved, so a missing or cyclic dependency fails here; but the spec keeps
        # what was asked for, not what it pulls in.
        with mock.patch.object(specs.repos, "canonical_features", return_value=("jvm",)), \
                mock.patch("bottle.features.resolve") as resolve:
            self.assertEqual(specs.canonical({"features": ["jvm"]})["features"], ["jvm"])
        resolve.assert_called_once_with(["jvm"])

    def test_is_its_own_canonical_form(self) -> None:
        spec = specs.canonical({"features": ["tools", "jvm:version=21"], "memory": "2048m"})
        self.assertEqual(specs.canonical(spec), spec)

    def test_refuses(self) -> None:
        for data, message in (
            ([], "a spec is a JSON object"),
            ({"feature": ["tools"]}, "unknown key feature"),
            ({"features": "tools"}, "features is a list"),
            ({"features": [1]}, "features is a list"),
            ({"features": ["nope"]}, "no feature named 'nope'"),
            ({"features": ["jvm:version=21", "jvm"]}, "given twice with different options"),
            ({"features": ["teamcity"]}, "needs its server option"),
            ({"memory": 8}, "memory is a string"),
            ({"memory": "lots"}, "invalid memory"),
            ({"memory": "64G"}, "more than this machine has"),
        ):
            with self.subTest(data), self.assertRaisesRegex(BottleError, message):
                specs.canonical(data)

    def test_memory_limit(self) -> None:
        self.assertIsNone(specs.memory_limit({"features": [], "memory": "all"}))
        self.assertEqual(specs.memory_limit({"features": [], "memory": "8G"}), "8G")


class ReadTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.path = Path(tmp.name) / "spec.json"

    def test_reads_the_canonical_form(self) -> None:
        self.path.write_text(json.dumps({"features": ["tools"]}))
        self.assertEqual(specs.read(self.path), {"features": ["tools"], "memory": "all"})

    def test_names_the_file_in_errors(self) -> None:
        for text, message in (("nope", "isn't valid JSON"), ('{"features": ["nope"]}', "no feature named")):
            self.path.write_text(text)
            with self.subTest(text), self.assertRaisesRegex(BottleError, f"spec {self.path}.*{message}"):
                specs.read(self.path)

    def test_a_missing_file(self) -> None:
        with self.assertRaisesRegex(BottleError, f"can't read spec {self.path}"):
            specs.read(self.path)
