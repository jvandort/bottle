import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

from bottle import specs
from bottle.errors import BottleError


class CanonicalTest(unittest.TestCase):
    def setUp(self) -> None:
        patcher = mock.patch.object(specs, "host_memory_mib", return_value=16384)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_features_are_sorted_with_defaults_left_out(self) -> None:
        spec = specs.canonical({"features": ["tools", "jvm:version=25,additionalVersions=17,21", "claude"], "memory": "2G"})
        self.assertEqual(spec, {"features": ["claude", "jvm:additionalVersions=17,21", "tools"], "memory": "2G"})

    def test_memory_as_container_takes_it(self) -> None:
        self.assertEqual(specs.canonical({"memory": "8g"})["memory"], "8G")

    def test_a_spec_gives_its_memory(self) -> None:
        self.assertEqual(specs.canonical({"memory": "2G"}), {"features": [], "memory": "2G"})
        for data in ({}, {"features": ["tools"]}, {"memory": "all"}):
            with self.subTest(data), self.assertRaises(BottleError):
                specs.canonical(data)

    def test_dependencies_are_left_to_the_feature(self) -> None:
        # Resolved, so a missing or cyclic dependency fails here; but the spec keeps
        # what was asked for, not what it pulls in.
        with mock.patch.object(specs, "canonical_features", return_value=("jvm",)), \
                mock.patch("bottle.features.resolve") as resolve:
            self.assertEqual(specs.canonical({"features": ["jvm"], "memory": "2G"})["features"], ["jvm"])
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
            ({}, "a spec needs its memory"),
            ({"memory": 8}, "memory is a string"),
            ({"memory": "lots"}, "invalid memory"),
            ({"memory": "64G"}, "more than this machine has"),
        ):
            with self.subTest(data), self.assertRaisesRegex(BottleError, message):
                specs.canonical(data)

    def test_memory_limit(self) -> None:
        self.assertEqual(specs.memory_limit({"features": [], "memory": "8G"}), "8G")


class ReadTest(unittest.TestCase):
    def setUp(self) -> None:
        patcher = mock.patch.object(specs, "host_memory_mib", return_value=16384)
        patcher.start()
        self.addCleanup(patcher.stop)
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.path = Path(tmp.name) / "spec.json"

    def test_reads_the_canonical_form(self) -> None:
        self.path.write_text(json.dumps({"features": ["tools"], "memory": "2g"}))
        self.assertEqual(specs.read(self.path), {"features": ["tools"], "memory": "2G"})

    def test_names_the_file_in_errors(self) -> None:
        for text, message in (("nope", "isn't valid JSON"), ('{"features": ["nope"]}', "no feature named")):
            self.path.write_text(text)
            with self.subTest(text), self.assertRaisesRegex(BottleError, f"spec {self.path}.*{message}"):
                specs.read(self.path)

    def test_a_missing_file(self) -> None:
        with self.assertRaisesRegex(BottleError, f"can't read spec {self.path}"):
            specs.read(self.path)
