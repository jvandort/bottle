import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import ca, daemon, features, images
from bottle.errors import BottleError
from tests.support import GitTestCase


class TagTest(unittest.TestCase):
    def test_images_are_named_in_full(self) -> None:
        self.assertEqual(images.tag("base"), "localhost/bottle/base:latest")


class RealBaseTest(unittest.TestCase):
    """The base image as this repo has it: the base feature on Debian."""

    def test_its_recipe(self) -> None:
        from bottle import features

        recipe = features.base_recipe()
        self.assertEqual(recipe["pull"], images.DEBIAN)
        apt, install, env = recipe["steps"][:3]
        self.assertIn("install -y --no-install-recommends ca-certificates git openssh-server sudo tini", apt["run"][2])
        self.assertEqual(install["run"], ["/bin/sh", "./install.sh"])
        self.assertEqual(env["env"]["LANG"], "C.UTF-8")


class InterpreterTest(unittest.TestCase):
    def test_a_script_is_run_by_its_hash_bang_line(self) -> None:
        # A read-only mount can't be made executable.
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "install.sh"
            for first, argv in (("#!/bin/bash -e", ["/bin/bash", "-e"]), ("#!/bin/sh", ["/bin/sh"]), ("set -e", ["/bin/sh"])):
                script.write_text(f"{first}\n")
                self.assertEqual(images.interpreter(script), [*argv, "./install.sh"])


class StalenessTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        (self.dir / "install.sh").write_text("apt-get install a\n")

    def test_tree_hash_tracks_names_contents_and_the_executable_bit(self) -> None:
        before = images.tree_hash(self.dir)
        self.assertEqual(images.tree_hash(self.dir), before)
        (self.dir / "install.sh").write_text("apt-get install b\n")
        after_edit = images.tree_hash(self.dir)
        (self.dir / "install.sh").chmod(0o755)
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


class StoreTest(GitTestCase):
    """Bottle's images: stages bottle-cli builds, each named for what it's made from, kept while one is used."""

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[tuple] = []
        self.recipes: list[dict] = []
        self.build_status = 0
        self.context = self.tmp / "context"
        self.context.mkdir()
        (self.context / "install.sh").write_text("#!/bin/sh\n")

        def bind(bottle, *args, **kwargs):
            self.calls.append(("bind", bottle))
            self.bound = kwargs
            return "127.0.0.1:47329", "token"

        for target, name, fake in (
            (daemon, "bind_decider", bind),
            (daemon, "release_egress", lambda bottle: self.calls.append(("release", bottle))),
            (images.subprocess, "run", self.cli),
            (images.prereqs, "ensure_cli", lambda: None),
            (ca, "certificate", lambda bottle, hosts: f"PEM for {bottle} of {','.join(hosts)}" if hosts else None),
            (features, "apt_https_hosts", lambda: ["apt.example"]),
        ):
            patcher = mock.patch.object(target, name, fake)
            patcher.start()
            self.addCleanup(patcher.stop)

    def cli(self, command, stdout=None, text=False, env=None, capture_output=False):
        """bottle-cli: a pull or a build makes a stage named for its arguments; gc deletes what it isn't told to keep."""
        args = command[1:]
        store = images.store()
        if args[0] == "gc":
            keep = {Path(k).name for k in args[2:]}
            for disk in store.glob("*.ext4"):
                if disk.name not in keep:
                    disk.unlink()
            return subprocess.CompletedProcess(command, 0, "", "")
        if args[0] == "pull":
            name = "pulled"
        else:
            self.calls.append(("build", args[3:], env.get("BOTTLE_DECIDER_TOKEN")))
            recipe = json.loads(Path(args[1]).read_text())
            self.recipes.append(recipe)
            if self.build_status:
                return subprocess.CompletedProcess(command, self.build_status, "", "")
            name = recipe["labels"]["bottle.inputs"][:8] + recipe["labels"].get("bottle.build", "")[:4]
        (store / f"{name}.ext4").write_text("")
        return subprocess.CompletedProcess(command, 0, f"{store}/below.ext4\n{store}/{name}.ext4\n", "")

    def build(self, tag: str, inputs: str, recipe: dict | None = None, no_cache: bool = False) -> Path:
        recipe = recipe or {"pull": "debian:13", "steps": [{"run": ["true"]}]}
        with mock.patch("sys.stderr"):
            return images.build_recipe(self.context, tag, inputs, recipe, no_cache)

    def test_a_build_runs_bound_like_a_bottle_and_released_after(self) -> None:
        self.build("localhost/bottle/base:latest", "aaaa" * 16)
        [bind, build, release] = self.calls
        self.assertEqual(bind[0], "bind")
        self.assertEqual(build, ("build", ["--decider", "127.0.0.1:47329"], "token"))
        self.assertEqual(release, ("release", bind[1]))

    def test_the_recipe_mounts_the_context_and_is_on_what_it_says(self) -> None:
        self.build("localhost/bottle/base:latest", "aaaa" * 16,
                   {"pull": "debian:13", "steps": [{"run": ["true"]}], "cmd": ["sshd"], "labels": {"title": "t"}})
        [recipe] = self.recipes
        self.assertEqual(recipe["from"], "pulled")
        self.assertEqual(recipe["mount"], {"source": str(self.context.resolve()), "target": images.MOUNT})
        self.assertEqual((recipe["steps"], recipe["cmd"]), ([{"run": ["true"]}], ["sshd"]))
        self.assertEqual(recipe["labels"], {"title": "t", "bottle.inputs": "aaaa" * 16})
        self.assertNotIn("entrypoint", recipe)  # what it's built on runs, then
        self.build("localhost/bottle/base:with-tools", "bbbb" * 16,
                   {"from": "localhost/bottle/base:latest", "steps": []})
        self.assertEqual(self.recipes[-1]["from"], "aaaaaaaa")

    def test_a_build_is_decided_as_a_build_of_its_features_and_its_apt_step_gets_the_builds_ca(self) -> None:
        recipe = {"pull": "debian:13", "features": ["claude"], "steps": [
            {"run": ["sh", "-c", "apt-get ..."], "env": {"HOME": "/root", images.BUILD_CA: ""}},
            {"run": ["./install.sh"], "env": {"HOME": "/root"}},
        ]}
        self.build("localhost/bottle/base:with-claude", "aaaa" * 16, recipe)
        self.assertEqual(self.bound, {"features": ["claude"], "build": True})
        apt_step, install = self.recipes[-1]["steps"]
        self.assertEqual(apt_step["env"][images.BUILD_CA], "PEM for bottle-builds of apt.example")
        self.assertNotIn(images.BUILD_CA, install["env"])
        self.assertNotIn("features", self.recipes[-1])  # bottle's, not bottle-cli's
        # The same CA for every build: built again from the same, the same stage.
        self.build("localhost/bottle/base:with-claude", "aaaa" * 16, recipe)
        self.assertEqual(self.recipes[-1]["steps"], self.recipes[-2]["steps"])

    def test_a_build_is_kept_under_its_tag(self) -> None:
        disk = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertEqual(disk.name, "aaaaaaaa.ext4")
        self.assertEqual(images.archive_of("localhost/bottle/base:latest"), disk)
        self.assertEqual(images.built_labels("localhost/bottle/base:latest"), {"bottle.inputs": "aaaa" * 16})

    def test_no_cache_makes_the_stage_anew(self) -> None:
        first = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        again = self.build("localhost/bottle/base:latest", "aaaa" * 16, no_cache=True)
        self.assertNotEqual(first, again)

    def test_on_an_image_that_isnt_built_is_an_error(self) -> None:
        with self.assertRaisesRegex(BottleError, "isn't built"):
            self.build("t", "inputs", {"from": "localhost/bottle/base:latest", "steps": []})

    def test_a_rebuild_leaves_the_stage_a_machine_was_made_from(self) -> None:
        old = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        new = self.build("localhost/bottle/base:latest", "bbbb" * 16)
        self.assertNotEqual(old, new)
        images.remove_unused({old.name})
        self.assertTrue(old.exists() and new.exists())
        images.remove_unused(set())
        self.assertFalse(old.exists())
        self.assertTrue(new.exists())

    def test_a_failed_build_is_an_error_keeps_nothing_and_releases_its_token(self) -> None:
        self.build_status = 1
        with self.assertRaisesRegex(BottleError, "bottle-cli build"):
            self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertIsNone(images.archive_of("localhost/bottle/base:latest"))
        self.assertEqual(self.calls[-1][0], "release")


if __name__ == "__main__":
    unittest.main()
