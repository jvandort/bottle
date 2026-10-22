import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import daemon, images
from bottle.errors import BottleError
from tests.support import GitTestCase


class TagTest(unittest.TestCase):
    def test_images_are_named_in_full(self) -> None:
        # On no registry: a name without a host would be taken to be Docker Hub's.
        self.assertEqual(images.tag("base"), "localhost/bottle/base:latest")
        self.assertEqual(images.FROM_BOTTLE.findall("FROM localhost/bottle/base:latest\nFROM bottle/tools\n"), ["base"])


class DependencyTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.containers = Path(tmp.name)
        patcher = mock.patch.object(images, "IMAGES", self.containers)
        patcher.start()
        self.addCleanup(patcher.stop)

    def image(self, name: str, dockerfile: str) -> None:
        (self.containers / name).mkdir()
        (self.containers / name / "Dockerfile").write_text(dockerfile)

    def test_real_images(self) -> None:
        self.assertEqual(images.available.__module__, "bottle.images")
        with mock.patch.object(images, "IMAGES", Path(images.__file__).resolve().parent.parent / "containers" / "images"):
            self.assertIn("base", images.available())
            self.assertEqual(images.dependencies("base"), [])

    def test_from_lines(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("a", "FROM --platform=linux/arm64 localhost/bottle/base:latest AS builder\nFROM localhost/bottle/base\n")
        self.assertEqual(images.dependencies("a"), ["base"])
        self.assertEqual(images.dependencies("base"), [])

    def test_order_is_dependencies_first(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("tools", "FROM localhost/bottle/base:latest\n")
        self.image("agent", "FROM localhost/bottle/tools:latest\n")
        self.image("multi", "FROM localhost/bottle/agent:latest AS a\nFROM localhost/bottle/tools:latest\n")
        self.assertEqual(images.build_order("multi"), ["base", "tools", "agent", "multi"])

    def test_cycle(self) -> None:
        self.image("a", "FROM localhost/bottle/b\n")
        self.image("b", "FROM localhost/bottle/a\n")
        with self.assertRaisesRegex(BottleError, "cycle: a -> b -> a"):
            images.build_order("a")

    def test_missing_dependency(self) -> None:
        self.image("a", "FROM localhost/bottle/ghost\n")
        with self.assertRaisesRegex(BottleError, "a is built from localhost/bottle/ghost, but there's no containers/images/ghost"):
            images.build_order("a")

    def test_ensure_built_builds_whats_missing_or_stale(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("tools", "FROM localhost/bottle/base:latest\n")
        built = []
        for exists, expected in ((set(), ["base", "tools"]), ({"localhost/bottle/base:latest"}, ["tools"]),
                                 ({"localhost/bottle/base:latest", "localhost/bottle/tools:latest"}, [])):
            built.clear()
            with self.subTest(exists=exists), \
                    mock.patch.object(images, "is_current", side_effect=lambda t, inputs: t in exists), \
                    mock.patch.object(images, "_build_one", side_effect=lambda n, no_cache=False: built.append(n)), \
                    mock.patch("sys.stderr"):
                images.ensure_built("tools")
                self.assertEqual(built, expected)

    def test_build_rebuilds_the_image_but_not_built_dependencies(self) -> None:
        self.image("base", "FROM debian:13\n")
        self.image("tools", "FROM localhost/bottle/base:latest\n")
        built = []
        with mock.patch.object(images, "is_current", return_value=True), \
                mock.patch.object(images, "_build_one", side_effect=lambda n, no_cache=False: built.append((n, no_cache))):
            self.assertEqual(images.build("tools", no_cache=True), "localhost/bottle/tools:latest")
        self.assertEqual(built, [("tools", True)])


class StalenessTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        (self.dir / "Dockerfile").write_text("FROM debian:13\n")

    def test_tree_hash_tracks_names_contents_and_the_executable_bit(self) -> None:
        before = images.tree_hash(self.dir)
        self.assertEqual(images.tree_hash(self.dir), before)
        (self.dir / "Dockerfile").write_text("FROM debian:14\n")
        after_edit = images.tree_hash(self.dir)
        (self.dir / "Dockerfile").chmod(0o755)
        after_chmod = images.tree_hash(self.dir)
        (self.dir / "extra").write_text("")
        self.assertEqual(len({before, after_edit, after_chmod, images.tree_hash(self.dir)}), 4)

    def test_is_current_compares_the_label(self) -> None:
        with mock.patch.object(images, "built_labels", return_value={"bottle.inputs": "abc"}):
            self.assertTrue(images.is_current("localhost/bottle/base:latest", "abc"))
            self.assertFalse(images.is_current("localhost/bottle/base:latest", "def"))
        with mock.patch.object(images, "built_labels", return_value=None):
            self.assertFalse(images.is_current("localhost/bottle/base:latest", "abc"))  # missing
        with mock.patch.object(images, "built_labels", return_value={}):
            self.assertFalse(images.is_current("localhost/bottle/base:latest", "abc"))  # built before labels


class BuildImageTest(unittest.TestCase):
    def test_base_is_available(self) -> None:
        self.assertIn("base", images.available())

    def test_unknown_image(self) -> None:
        with mock.patch.object(images.prereqs, "ensure_smolvm") as ensure_smolvm, \
                self.assertRaisesRegex(BottleError, "no image named 'nope'; available: .*base"):
            images.build("nope")
        ensure_smolvm.assert_not_called()

    def test_an_image_is_built_by_smolvm_from_its_directory(self) -> None:
        with mock.patch.object(images.prereqs, "ensure_smolvm"), mock.patch.object(images, "_build") as build:
            self.assertEqual(images.build("base"), "localhost/bottle/base:latest")
        context, tag, labels = build.call_args.args[:3]
        self.assertEqual((context, tag), (images.IMAGES / "base", "localhost/bottle/base:latest"))
        self.assertEqual(labels, {images.INPUTS_LABEL: images.inputs_hash("base")})


class StoreTest(GitTestCase):
    """Bottle's images: built in a builder machine, kept as OCI archives named for their inputs, while one is used."""

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[tuple] = []
        self.scripts: list[str] = []
        self.sent: list[str] = []  # the names in the inputs' tar
        self.build_status, self.readable = 0, True
        contexts = self.tmp / "containers"
        (contexts / "images" / "base").mkdir(parents=True)
        (contexts / "images" / "base" / "Dockerfile").write_text("FROM debian\n")
        self.contexts = contexts

        def bind(bottle, *args, **kwargs):
            self.calls.append(("bind", bottle))
            return "127.0.0.1:47329", "token"

        for target, name, fake in (
            (images.smolvm, "create", lambda name, image, mounts, env, memory_mib, cpus:
                self.calls.append(("create", image, tuple(mounts)))),
            (images.smolvm, "start", lambda name, decider, token: self.calls.append(("start", decider, token))),
            (images.smolvm, "delete", lambda name: self.calls.append(("delete",))),
            (daemon, "bind_decider", bind),
            (daemon, "release_egress", lambda bottle: self.calls.append(("release", bottle))),
            (images.subprocess, "run", self.in_builder),
        ):
            patcher = mock.patch.object(target, name, fake)
            patcher.start()
            self.addCleanup(patcher.stop)

    def in_builder(self, command, stdin=None, stdout=None):
        """An exec in the builder: the build script, or reading the archive out."""
        argv = command[command.index("--") + 1:]
        if "tar -x" in argv[-1]:
            with tarfile.open(fileobj=stdin) as tar:
                self.sent = tar.getnames()
            return subprocess.CompletedProcess(command, 0)
        if argv[0] == "cat":
            if self.readable:
                stdout.write(b"the archive")
            return subprocess.CompletedProcess(command, 0 if self.readable else 1)
        self.scripts.append(argv[-1])
        return subprocess.CompletedProcess(command, self.build_status)

    def build(self, tag: str, inputs: str, bases=None) -> Path:
        return images._build(self.contexts / "images" / "base", tag, {"bottle.inputs": inputs}, bases=bases)

    def test_a_build_runs_in_a_machine_bound_like_a_bottle_and_gone_after(self) -> None:
        self.build("localhost/bottle/base:latest", "aaaa" * 16)
        [create, bind, start, release, delete] = self.calls
        self.assertEqual(create, ("create", images.BUILDER_IMAGE, ()))  # no mounts: everything goes over exec
        self.assertEqual(bind[0], "bind")
        self.assertEqual(start, ("start", "127.0.0.1:47329", "token"))
        self.assertEqual((release, delete), (("release", bind[1]), ("delete",)))

    def test_its_inputs_go_in_over_stdin(self) -> None:
        base = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.build("localhost/bottle/base:with-tools", "bbbb" * 16, bases={"localhost/bottle/base:latest": base})
        self.assertLessEqual({"context", "context/Dockerfile", "Dockerfile", f"bases/{base.name}"}, set(self.sent))
        self.assertEqual(list((self.bottle_home / "build").iterdir()), [])  # the tar sent is gone after

    def test_its_progress_is_on_stderr_and_the_archive_comes_out_on_stdout(self) -> None:
        archive = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertIn("exec >&2", self.scripts[-1])
        self.assertIn(f"oci-archive:{images.OUT}:", self.scripts[-1])
        self.assertEqual(archive.read_bytes(), b"the archive")

    def test_a_build_is_kept_under_its_inputs(self) -> None:
        archive = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertEqual(archive.name, "localhost_bottle_base_latest-aaaaaaaaaaaa.tar")
        self.assertEqual(images.archive_of("localhost/bottle/base:latest"), archive)
        self.assertEqual(images.built_labels("localhost/bottle/base:latest"), {"bottle.inputs": "aaaa" * 16})

    def test_the_builder_loads_the_images_it_builds_on(self) -> None:
        base = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.build("localhost/bottle/base:with-tools", "bbbb" * 16, bases={"localhost/bottle/base:latest": base})
        script = self.scripts[-1]
        self.assertIn(f"pull -q oci-archive:{images.INPUTS}/bases/{base.name}", script)
        self.assertIn('tag "$id" localhost/bottle/base:latest', script)
        self.assertLess(script.index("pull"), script.index(" build "))

    def test_a_rebuild_leaves_the_archive_a_machine_was_made_from(self) -> None:
        old = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        new = self.build("localhost/bottle/base:latest", "bbbb" * 16)
        self.assertNotEqual(old, new)
        images.remove_unused({old.name})
        self.assertTrue(old.exists() and new.exists())
        images.remove_unused(set())
        self.assertFalse(old.exists())
        self.assertTrue(new.exists())

    def test_a_failed_build_is_an_error_keeps_nothing_and_cleans_up(self) -> None:
        for what in ("build", "read"):
            with self.subTest(what):
                self.calls.clear()
                self.build_status, self.readable = (1, True) if what == "build" else (0, False)
                with self.assertRaises(BottleError):
                    self.build("localhost/bottle/base:latest", "aaaa" * 16)
                self.assertIsNone(images.archive_of("localhost/bottle/base:latest"))
                self.assertEqual(list(images.store().iterdir()), [])
                self.assertEqual([c[0] for c in self.calls[-2:]], ["release", "delete"])


class BuildContextTest(GitTestCase):
    def test_a_features_build_loads_the_base_it_is_built_from(self) -> None:
        dockerfile = self.tmp / "Dockerfile"
        dockerfile.write_text("FROM localhost/bottle/base:latest\nRUN true\n")
        base = self.tmp / "base.tar"
        with mock.patch.object(images, "archive_of", return_value=base) as archive_of, \
                mock.patch.object(images, "_build") as build, \
                mock.patch.object(images.prereqs, "ensure_smolvm"):
            images.build_context(self.tmp, "localhost/bottle/base:with-tools", "inputs", dockerfile=dockerfile,
                                 labels={"bottle.features": "tools"})
        archive_of.assert_called_with("localhost/bottle/base:latest")
        args = build.call_args.args
        self.assertEqual(args[1:3], ("localhost/bottle/base:with-tools",
                                     {"bottle.inputs": "inputs", "bottle.features": "tools"}))
        self.assertEqual(args[5], {"localhost/bottle/base:latest": base})

    def test_a_base_that_isnt_built_is_an_error(self) -> None:
        dockerfile = self.tmp / "Dockerfile"
        dockerfile.write_text("FROM localhost/bottle/base:latest\n")
        with mock.patch.object(images, "archive_of", return_value=None), \
                mock.patch.object(images.prereqs, "ensure_smolvm"), \
                self.assertRaisesRegex(BottleError, "isn't built"):
            images.build_context(self.tmp, "t", "inputs", dockerfile=dockerfile)


if __name__ == "__main__":
    unittest.main()
