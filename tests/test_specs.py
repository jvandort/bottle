import io
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
            ({"memory": "2G", "permissions": "message-any"}, "permissions is a list"),
            ({"memory": "2G", "permissions": ["root"]}, "unknown permission root; there are list-bottles, message-any"),
            ({"memory": "2G", "thread": "Fix It"}, "invalid thread"),
            ({"memory": "2G", "thread": 1}, "invalid thread"),
        ):
            with self.subTest(data), self.assertRaisesRegex(BottleError, message):
                specs.canonical(data)

    def test_permissions_are_sorted_once_each_and_left_out_when_none(self) -> None:
        spec = specs.canonical({"memory": "2G", "permissions": ["message-any", "list-bottles", "message-any"]})
        self.assertEqual(spec["permissions"], ["list-bottles", "message-any"])
        self.assertNotIn("permissions", specs.canonical({"memory": "2G", "permissions": []}))
        self.assertEqual(specs.canonical(spec), spec)

    def test_a_thread(self) -> None:
        self.assertEqual(specs.canonical({"memory": "2G", "thread": "fix-it"})["thread"], "fix-it")
        self.assertNotIn("thread", specs.canonical({"memory": "2G"}))

    def test_a_disk_in_whole_g_or_none(self) -> None:
        spec = specs.canonical({"memory": "2G", "disk": "50g"})
        self.assertEqual((spec["disk"], specs.disk_gib(spec)), ("50G", 50))
        self.assertEqual(specs.canonical(spec), spec)
        self.assertIsNone(specs.disk_gib(specs.canonical({"memory": "2G"})))   # smolvm's default, 20G
        for disk in ("50", "512M", "0G", 50, "1.5G"):
            with self.subTest(disk), self.assertRaisesRegex(BottleError, "invalid disk"):
                specs.canonical({"memory": "2G", "disk": disk})

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


class RunnerMemoryTest(unittest.TestCase):
    def test_a_spec_alone_can_ask_for_any_memory(self) -> None:
        # Which machine it's for is bottle new's to know, and to check it against.
        self.assertEqual(specs.canonical({"memory": "512G"})["memory"], "512G")

    def test_a_spec_for_a_runner_fits_the_runners_memory(self) -> None:
        self.assertEqual(specs.canonical({"memory": "8G"}, 16384, "runner box")["memory"], "8G")
        with self.assertRaisesRegex(BottleError, r"memory 32G is more than runner box has \(16384M\)"):
            specs.canonical({"memory": "32G"}, 16384, "runner box")

    def test_linux_says_its_memory_in_proc_meminfo(self) -> None:
        meminfo = io.StringIO("MemFree:  1000 kB\nMemTotal:       16384000 kB\n")
        with mock.patch.object(specs.platform, "system", return_value="Linux"), \
                mock.patch("builtins.open", return_value=meminfo):
            self.assertEqual(specs.host_memory_mib(), 16000)
