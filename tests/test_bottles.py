import json
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from bottle import bottles, repos
from bottle.errors import BottleError
from tests.support import GitTestCase, run


class DefaultNameTest(unittest.TestCase):
    def test_first_bottle_is_the_repo_name(self) -> None:
        self.assertEqual(bottles.default_name("gradle", set()), "gradle")

    def test_then_numbered_from_two(self) -> None:
        self.assertEqual(bottles.default_name("gradle", {"gradle"}), "gradle-2")
        self.assertEqual(bottles.default_name("gradle", {"gradle", "gradle-2"}), "gradle-3")

    def test_reuses_gaps(self) -> None:
        self.assertEqual(bottles.default_name("gradle", {"gradle", "gradle-3"}), "gradle-2")
        self.assertEqual(bottles.default_name("gradle", {"gradle-2"}), "gradle")


class FakeRuntime:
    """Stands in for runtime and daemon, recording what exists and what was done."""

    def __init__(self, fail: str | None = None, fail_cleanup: str | None = None) -> None:
        self.fail = fail
        self.fail_cleanup = fail_cleanup
        self.networks: set[str] = set()
        self.containers: dict[str, str] = {}
        self.egress: set[str] = set()
        self.container_images: dict[str, str] = {}
        self.container_labels: dict[str, dict] = {}
        self.container_networks: dict[str, str] = {}
        self.built_images: dict[str, str] = {}
        self.calls: list[str] = []
        self.statuses_seen: list[str] = []

    def _step(self, name: str) -> None:
        self.calls.append(name)
        if self.fail == name:
            raise BottleError(f"{name} failed")

    def network_create(self, network, labels=None):
        self.statuses_seen.append(bottles.load()[next(iter(bottles.load()))].status)
        self._step("network_create")
        self.networks.add(network)

    def network_gateway(self, network):
        return "192.168.128.1"

    def network_delete(self, network):
        if self.fail_cleanup == "network":
            raise BottleError("network busy")
        self.networks.discard(network)

    def container_run(self, name, image, network, env, mounts, labels=None):
        self._step("container_run")
        self.run_args = dict(name=name, image=image, network=network, env=env, mounts=mounts, labels=labels)
        self.containers[name] = "running"
        self.container_labels[name] = dict(labels or {})
        self.container_networks[name] = network
        self.container_images[name] = image
        self.built_images.setdefault(image, f"sha256:{image}")

    def images(self):
        return [bottles.runtime.ImageRef(tag, digest) for tag, digest in self.built_images.items()]

    def images_in_use(self):
        return {self.built_images[tag] for name, tag in self.container_images.items() if name in self.containers}

    def image_delete(self, tag):
        self.calls.append(f"image_delete {tag}")
        del self.built_images[tag]

    def network_exists(self, network):
        return network in self.networks

    def container_state(self, name):
        return self.containers.get(name)

    def container_start(self, name):
        self._step("container_start")
        self.containers[name] = "running"

    def container_stop(self, name):
        self.calls.append("container_stop")
        self.containers[name] = "stopped"

    def stop_daemon(self):
        self.calls.append("daemon_stop")
        return True

    def container_info(self, name):
        if name not in self.containers:
            return None
        return bottles.runtime.ContainerInfo(
            self.containers[name], self.container_labels.get(name, {}), [self.container_networks.get(name, "")])

    def container_delete(self, name, owner=None):
        info = self.container_info(name)
        if info is None:
            return
        if owner is not None and not bottles.runtime._owned(info, *owner):
            raise BottleError(f"container {name} isn't this bottle's; leaving it alone")
        self.containers.pop(name)

    contract_output = ""
    credentials: dict = {}

    def container_exec(self, name, argv, user=None, workdir=None, input=None):
        if "/etc/environment" in argv[-1] and input is not None:
            self._step("deliver_credentials")
            self.delivered = input
            return ""
        if "# bottle contract" in argv[-1]:
            self._step("verify_contract")
            return self.contract_output
        self._step("init_workspace")
        self.workspace_script = argv[-1]
        return ""

    def ensure_egress(self, bottle, network, git_dir=None):
        self._step("ensure_egress")
        self.egress_git_dir = git_dir
        self.egress.add(bottle)
        return "http://192.168.128.1:3128"

    def release_egress(self, bottle):
        self.egress.discard(bottle)

    def patch(self, test: unittest.TestCase) -> None:
        for module, names in (
            (bottles.runtime, ("network_create", "network_gateway", "network_delete", "network_exists",
                               "container_run", "container_state", "container_start", "container_stop",
                               "container_delete", "container_info", "container_exec", "images", "images_in_use",
                               "image_delete")),
            (bottles.daemon, ("ensure_egress", "release_egress", "stop")),
        ):
            for name in names:
                patcher = mock.patch.object(module, name, getattr(self, "stop_daemon" if name == "stop" else name))
                patcher.start()
                test.addCleanup(patcher.stop)
        for patcher in (
            mock.patch.object(bottles.prereqs, "ensure_container"),
            mock.patch.object(bottles.images, "is_current", return_value=True),
            mock.patch.object(bottles.features_, "remove_stale", return_value=[]),
            mock.patch.object(bottles.auth, "env_for", side_effect=lambda specs: dict(self.credentials)),
            mock.patch.object(bottles.auth, "ensure_logged_in", side_effect=lambda specs: self.calls.append("ensure_logged_in")),
        ):
            patcher.start()
            test.addCleanup(patcher.stop)


class BottleTestCase(GitTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.repo_path = self.make_repo("gradle")
        self.head = run("git", "-C", self.repo_path, "rev-parse", "HEAD")
        repos.add(self.repo_path)

    def fake(self, **kwargs) -> FakeRuntime:
        fake = FakeRuntime(**kwargs)
        fake.patch(self)
        return fake


class CreateTest(BottleTestCase):
    def test_creates_every_part(self) -> None:
        fake = self.fake()

        bottle = bottles.create("gradle", "base")

        self.assertEqual((bottle.name, bottle.branch, bottle.commit, bottle.status), ("gradle", "main", self.head, "ready"))
        self.assertEqual(bottles.get("gradle"), bottle)
        self.assertEqual(fake.networks, {bottle.network})
        self.assertEqual(fake.containers, {bottle.container: "running"})
        self.assertEqual(fake.egress, {"gradle"})

    def test_egress_serves_the_repo_as_origin(self) -> None:
        fake = self.fake()
        bottles.create("gradle")
        self.assertEqual(fake.egress_git_dir, self.repo_path / ".git")
        self.assertIn("remote add origin http://bottle.host/git/origin", fake.workspace_script)
        self.assertIn("remote add host http://bottle.host/git/host", fake.workspace_script)
        self.assertIn("fetch -q --multiple origin host", fake.workspace_script)

    def test_order_container_before_egress(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "base")
        self.assertEqual(fake.calls, ["ensure_logged_in", "network_create", "container_run", "verify_contract", "ensure_egress", "init_workspace"])

    def test_record_is_written_before_anything_is_created(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "base")
        self.assertEqual(fake.statuses_seen, ["creating"])

    def test_container_gets_image_proxy_and_read_only_objects(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "base")
        self.assertEqual(fake.run_args["image"], "bottle/base:latest")
        self.assertEqual(fake.run_args["env"]["HTTPS_PROXY"], "http://192.168.128.1:3128")
        self.assertEqual(fake.run_args["env"]["http_proxy"], "http://192.168.128.1:3128")
        [mount] = fake.run_args["mounts"]
        self.assertEqual((mount.source, mount.target, mount.readonly), (self.repo_path / ".git/objects", "/mnt/repo/objects", True))

    def test_workspace_setup_writes_the_agent_context(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle")
        self.assertIn('> "$HOME/BOTTLE.md"', fake.workspace_script)
        context = bottles.context(bottle)
        self.assertIn("checkout of the `gradle` repo, on the `main` branch.", context)
        self.assertNotIn(self.head[:12], context)  # no commits: it never goes stale
        self.assertIn("You're `genie`, with passwordless sudo.", context)
        self.assertIn("only through the HTTP proxy", context)

    def test_context_for_a_detached_commit(self) -> None:
        bottle = bottles.Bottle("b", "id", "r", "base", None, "c" * 40, 0.0)
        self.assertIn("checkout of the `r` repo, at a detached commit.", bottles.context(bottle))

    def test_workspace_script_quotes_values(self) -> None:
        run("git", "-C", self.repo_path, "branch", "we$rd;branch")
        fake = self.fake()
        bottles.create("gradle", "base", branch="we$rd;branch")
        self.assertIn("git init -q -b 'we$rd;branch' /workspace", fake.workspace_script)

    def test_names_further_bottles(self) -> None:
        self.fake()
        names = [bottles.create("gradle", "base").name for _ in range(3)]
        self.assertEqual(names, ["gradle", "gradle-2", "gradle-3"])

    def test_detached_start(self) -> None:
        self.commit(self.repo_path, "later")
        run("git", "-C", self.repo_path, "checkout", "-q", "--detach", self.head)
        fake = self.fake()
        bottle = bottles.create("gradle", "base")
        self.assertEqual((bottle.branch, bottle.commit, bottle.checkout), (None, self.head, f"({self.head[:12]})"))
        self.assertIn(f"update-ref --no-deref HEAD {self.head}", fake.workspace_script)
        self.assertEqual(bottles.get("gradle"), bottle)  # survives the JSON round trip

    def test_explicit_name_and_branch(self) -> None:
        run("git", "-C", self.repo_path, "branch", "feature")
        self.fake()
        bottle = bottles.create("gradle", "base", branch="feature", name="mine")
        self.assertEqual((bottle.name, bottle.branch), ("mine", "feature"))

    def test_duplicate_name(self) -> None:
        self.fake()
        bottles.create("gradle", "base", name="mine")
        with self.assertRaisesRegex(BottleError, "already exists"):
            bottles.create("gradle", "base", name="mine")

    def test_unknown_image(self) -> None:
        self.fake()
        with self.assertRaisesRegex(BottleError, "no image named 'nope'"):
            bottles.create("gradle", "nope")
        self.assertEqual(bottles.load(), {})

    def test_builds_the_image_first(self) -> None:
        fake = self.fake()

        def ensure_built(image, feature_ids):
            fake.calls.append(f"ensure_built {image} {feature_ids}")
            return "bottle/base:with-tools"

        with mock.patch.object(bottles.features_, "ensure_built", side_effect=ensure_built):
            bottle = bottles.create("gradle", "base", features=["tools"])
        self.assertEqual(fake.calls[0], "ensure_built base ['tools']")
        self.assertEqual(fake.run_args["image"], "bottle/base:with-tools")
        self.assertEqual((bottle.features, bottle.environment), (("tools",), "base+tools"))
        self.assertEqual(bottles.get("gradle").features, ("tools",))  # survives the JSON round trip

    def test_records_feature_dependencies(self) -> None:
        self.fake()
        resolved = [SimpleNamespace(spec="tools"), SimpleNamespace(spec="jvm")]
        with mock.patch.object(bottles.features_, "resolve", return_value=resolved), \
                mock.patch.object(bottles.features_, "ensure_built", return_value="bottle/base:with-jvm.tools"):
            bottle = bottles.create("gradle", "base", features=["jvm"])
        self.assertEqual(bottle.features, ("tools", "jvm"))
        self.assertEqual(bottle.environment, "base+jvm+tools")

    def test_old_records_without_features_load(self) -> None:
        self.fake()
        bottles.create("gradle", "base")
        data = json.loads(bottles.registry_path().read_text())
        del data["bottles"]["gradle"]["features"]
        bottles.registry_path().write_text(json.dumps(data))
        self.assertEqual(bottles.get("gradle").features, ())

    def test_unknown_repo(self) -> None:
        with self.assertRaisesRegex(BottleError, "no repo named 'nope' \\(repos: gradle\\)"):
            bottles.create("nope", "tools")

    def test_unknown_branch(self) -> None:
        self.fake()
        with self.assertRaisesRegex(BottleError, "has no branch 'nope'"):
            bottles.create("gradle", "base", branch="nope")


class RollbackTest(BottleTestCase):
    def test_failure_at_each_step_leaves_nothing(self) -> None:
        for step in ("network_create", "container_run", "verify_contract", "ensure_egress", "init_workspace"):
            with self.subTest(step):
                fake = self.fake(fail=step)
                with self.assertRaisesRegex(BottleError, f"{step} failed"):
                    bottles.create("gradle", "base")
                self.assertEqual(bottles.load(), {})
                self.assertEqual((fake.networks, fake.containers, fake.egress), (set(), {}, set()))

    def test_interrupt_rolls_back(self) -> None:
        fake = self.fake()
        with mock.patch.object(bottles.runtime, "container_run", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                bottles.create("gradle", "base")
        self.assertEqual(bottles.load(), {})
        self.assertEqual(fake.networks, set())

    def test_failed_cleanup_keeps_a_broken_record(self) -> None:
        self.fake(fail="init_workspace", fail_cleanup="network")
        with self.assertRaisesRegex(BottleError, "init_workspace failed; cleanup also failed: .*network busy"):
            bottles.create("gradle", "base")
        self.assertEqual(bottles.get("gradle").status, "broken")


class DeleteTest(BottleTestCase):
    def test_removes_every_part(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "base")
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})
        self.assertEqual((fake.networks, fake.containers, fake.egress), (set(), {}, set()))

    def test_finishes_off_a_half_made_bottle(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "base")
        # As if creation died after the network: no container, no egress.
        fake.containers.clear()
        fake.egress.clear()
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})
        self.assertNotIn(bottle.network, fake.networks)

    def test_retry_after_a_failed_delete(self) -> None:
        fake = self.fake(fail_cleanup="network")
        bottles.create("gradle", "base")
        with self.assertRaisesRegex(BottleError, "rerun `bottle delete gradle`"):
            bottles.delete("gradle")
        self.assertEqual(bottles.get("gradle").status, "broken")
        self.assertEqual(fake.containers, {})  # later steps still ran

        fake.fail_cleanup = None
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})

    def test_unknown_bottle(self) -> None:
        with self.assertRaisesRegex(BottleError, "no bottle named 'nope'"):
            bottles.delete("nope")


class EnsureRunningTest(BottleTestCase):
    def test_starts_a_stopped_bottle_and_its_egress(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "base")
        fake.containers[bottle.container] = "stopped"
        fake.egress.clear()

        bottles.ensure_running("gradle")

        self.assertEqual(fake.containers[bottle.container], "running")
        self.assertEqual(fake.egress, {"gradle"})

    def test_starting_verifies_the_contract(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle")
        fake.containers[bottle.container] = "stopped"
        fake.calls.clear()
        bottles.ensure_running("gradle")
        self.assertEqual(fake.calls, ["container_start", "verify_contract", "ensure_egress"])

    def test_running_bottle_only_ensures_egress(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "base")
        fake.calls.clear()
        bottles.ensure_running("gradle")
        self.assertEqual(fake.calls, ["ensure_egress"])

    def test_missing_container(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "base")
        fake.containers.clear()
        with self.assertRaisesRegex(BottleError, "container is gone; recreate it with `bottle reset gradle`, or `bottle delete gradle`"):
            bottles.ensure_running("gradle")

    def test_unfinished_bottle(self) -> None:
        self.fake(fail="init_workspace", fail_cleanup="network")
        with self.assertRaises(BottleError):
            bottles.create("gradle", "base")
        with self.assertRaisesRegex(BottleError, "gradle is broken"):
            bottles.ensure_running("gradle")


class ContractTest(BottleTestCase):
    def test_every_check_has_a_description(self) -> None:
        self.assertTrue(all(what and test for what, test in bottles.CONTRACT))

    def test_a_broken_contract_fails_creation_and_rolls_back(self) -> None:
        fake = self.fake()
        fake.contract_output = "git is installed\n/workspace exists and genie can write to it\n"
        with self.assertRaisesRegex(
            BottleError, "base doesn't meet the bottle contract: git is installed; /workspace exists and genie can write to it"
        ):
            bottles.create("gradle")
        self.assertEqual((bottles.load(), fake.containers), ({}, {}))

    def test_image_defaults_to_base(self) -> None:
        fake = self.fake()
        self.assertEqual(bottles.create("gradle").image, "base")
        self.assertEqual(fake.run_args["image"], "bottle/base:latest")

    def run_contract_script(self):
        """Run the script verify_contract would have run in a bottle, on this machine instead."""
        script = None

        def capture(name, argv, user=None, workdir=None):
            nonlocal script
            script = argv
            return ""

        bottle = bottles.Bottle("b", "id", "r", "base", "main", "c" * 40, 0.0)
        with mock.patch.object(bottles.runtime, "container_exec", side_effect=capture):
            bottles.verify_contract(bottle)
        return __import__("subprocess").run(script, capture_output=True, text=True)

    def test_checks_run_in_the_bottle(self) -> None:
        # Run the real checks against this machine: the script itself must be well-formed.
        # Which of them fail depends on where the suite runs -- a Mac fails most, a bottle
        # running the suite on itself fails none -- so only require that whatever it reports
        # is contract names and nothing else (no shell errors, no half-quoted lines).
        result = self.run_contract_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLessEqual(set(result.stdout.splitlines()), {what for what, _ in bottles.CONTRACT})

    def test_checks_name_what_failed(self) -> None:
        # That the script can report at all, without depending on this machine failing a check.
        with mock.patch.object(bottles, "CONTRACT", (("this passes", "true"), ("this fails", "false"))):
            result = self.run_contract_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["this fails"])


class RepoDefaultsTest(BottleTestCase):
    def test_merge(self) -> None:
        defaults = ("tools", "jvm:version=21", "claude")
        self.assertEqual(bottles.merge_features(defaults, []), ["tools", "jvm:version=21", "claude"])
        self.assertEqual(bottles.merge_features(defaults, ["node"]), ["tools", "jvm:version=21", "claude", "node"])
        self.assertEqual(bottles.merge_features(defaults, ["jvm:version=17"]), ["tools", "jvm:version=17", "claude"])

    def test_new_uses_the_repos_features(self) -> None:
        self.fake()
        repos.set_settings("gradle", ["tools", "jvm:version=21"])
        with mock.patch.object(bottles.features_, "ensure_built", return_value="bottle/base:with-x"):
            bottle = bottles.create("gradle")
        self.assertEqual(sorted(bottle.features), ["jvm:version=21", "tools"])

    def test_existing_bottles_keep_their_features(self) -> None:
        self.fake()
        repos.set_settings("gradle", ["tools"])
        with mock.patch.object(bottles.features_, "ensure_built", return_value="bottle/base:with-x"):
            bottles.create("gradle")
        repos.set_settings("gradle", ["claude"])
        self.assertEqual(bottles.get("gradle").features, ("tools",))


class CredentialDeliveryTest(BottleTestCase):
    def test_bottles_without_credentials_get_nothing(self) -> None:
        fake = self.fake()
        bottles.create("gradle")
        self.assertNotIn("deliver_credentials", fake.calls)

    def test_delivered_at_creation_via_stdin(self) -> None:
        fake = self.fake()
        fake.credentials = {"CLAUDE_CODE_OAUTH_TOKEN": "tok", "OTHER": None}
        bottles.create("gradle")
        self.assertEqual(fake.calls[-1], "deliver_credentials")
        self.assertEqual(fake.delivered, "CLAUDE_CODE_OAUTH_TOKEN=tok\nOTHER=\n")  # unset ones are cleared

    def test_delivered_again_when_a_stopped_bottle_starts(self) -> None:
        fake = self.fake()
        fake.credentials = {"CLAUDE_CODE_OAUTH_TOKEN": "tok"}
        bottle = bottles.create("gradle")
        fake.containers[bottle.container] = "stopped"
        fake.credentials = {"CLAUDE_CODE_OAUTH_TOKEN": "new"}
        bottles.ensure_running("gradle")
        self.assertEqual(fake.delivered, "CLAUDE_CODE_OAUTH_TOKEN=new\n")

    def test_new_start_reset_and_shell_log_in_first(self) -> None:
        fake = self.fake()
        bottles.create("gradle")
        self.assertEqual(fake.calls[0], "ensure_logged_in")
        for action in (bottles.start, bottles.reset):
            fake.calls.clear()
            action("gradle")
            self.assertEqual(fake.calls[0], "ensure_logged_in", action.__name__)
        fake.calls.clear()
        with mock.patch.object(bottles.runtime, "container_exec_interactive"):
            bottles.shell("gradle")
        self.assertEqual(fake.calls[0], "ensure_logged_in")

    def test_shell_passes_set_credentials_by_name(self) -> None:
        fake = self.fake()
        fake.credentials = {"CLAUDE_CODE_OAUTH_TOKEN": "tok", "OTHER": None}
        bottles.create("gradle")
        with mock.patch.object(bottles.runtime, "container_exec_interactive") as interactive:
            bottles.shell("gradle")
        self.assertEqual(interactive.call_args.kwargs["env"], {"CLAUDE_CODE_OAUTH_TOKEN": "tok"})


class OwnershipTest(BottleTestCase):
    """Bottle must never touch a container it didn't make (a scratch home's rollback once did)."""

    def someone_elses(self, fake, name: str) -> None:
        fake.containers[name] = "running"
        fake.container_labels[name] = {"bottle.id": "someone-else"}
        fake.container_networks[name] = "their-network"

    def test_new_refuses_a_taken_name_and_leaves_it_alone(self) -> None:
        fake = self.fake()
        self.someone_elses(fake, "bottle-" + bottles.namespace() + "gradle")
        with self.assertRaisesRegex(BottleError, "already exists; choose another name"):
            bottles.create("gradle")
        self.assertEqual(fake.containers, {"bottle-" + bottles.namespace() + "gradle": "running"})
        self.assertEqual(bottles.load(), {})
        self.assertEqual(fake.calls, ["ensure_logged_in"])  # nothing was created, so nothing rolled back

    def test_delete_leaves_a_container_that_isnt_the_bottles(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle")
        self.someone_elses(fake, bottle.container)  # replaced behind bottle's back
        with self.assertRaisesRegex(BottleError, "isn't this bottle's"):
            bottles.delete("gradle", force=True)
        self.assertIn(bottle.container, fake.containers)
        self.assertEqual(bottles.get("gradle").status, "broken")

    def test_containers_from_before_labels_are_recognised_by_network(self) -> None:
        info = bottles.runtime.ContainerInfo("running", {}, ["bottle-abc"])
        self.assertTrue(bottles.runtime._owned(info, "abc", "bottle-abc"))
        self.assertFalse(bottles.runtime._owned(info, "abc", "bottle-other"))

    def test_the_label_decides_when_present(self) -> None:
        info = bottles.runtime.ContainerInfo("running", {"bottle.id": "abc"}, ["bottle-other"])
        self.assertTrue(bottles.runtime._owned(info, "abc", "bottle-abc"))
        self.assertFalse(bottles.runtime._owned(info, "xyz", "bottle-other"))

    def test_containers_are_labelled(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle")
        self.assertEqual(fake.run_args["labels"], {"bottle.id": bottle.id})

    def test_other_homes_get_their_own_container_names(self) -> None:
        # The tests run in a scratch BOTTLE_HOME, so names are prefixed.
        self.assertRegex(bottles.namespace(), r"^[0-9a-f]{6}-$")
        with mock.patch.dict(__import__("os").environ, {"BOTTLE_HOME": ""}):
            self.assertEqual(bottles.namespace(), "")  # empty means the default home


class DeleteImageTest(BottleTestCase):
    def create_with(self, fake, features, name=None):
        tag = f"bottle/base:with-{'.'.join(sorted(features))}" if features else "bottle/base:latest"
        with mock.patch.object(bottles.features_, "ensure_built", return_value=tag), \
                mock.patch.object(bottles.features_, "resolve", side_effect=lambda specs: [SimpleNamespace(id=s, spec=s, overrides=()) for s in specs]), \
                mock.patch.object(bottles.features_, "image_tag", return_value=tag):
            return bottles.create("gradle", features=features, name=name), tag

    def test_delete_removes_the_bottles_image(self) -> None:
        fake = self.fake()
        bottle, tag = self.create_with(fake, ["tools"])
        with mock.patch.object(bottles.features_, "resolve", return_value=[]), \
                mock.patch.object(bottles.features_, "image_tag", return_value=tag):
            bottles.delete("gradle")
        self.assertNotIn(tag, fake.built_images)

    def test_keeps_an_image_another_bottle_uses(self) -> None:
        fake = self.fake()
        _, tag = self.create_with(fake, ["tools"])
        self.create_with(fake, ["tools"], name="other")
        with mock.patch.object(bottles.features_, "resolve", return_value=[]), \
                mock.patch.object(bottles.features_, "image_tag", return_value=tag):
            bottles.delete("gradle")
        self.assertIn(tag, fake.built_images)

    def test_keeps_the_base_image(self) -> None:
        fake = self.fake()
        self.create_with(fake, [])
        bottles.delete("gradle")
        self.assertIn("bottle/base:latest", fake.built_images)

    def test_rollback_keeps_the_freshly_built_image(self) -> None:
        fake = self.fake(fail="init_workspace")
        with self.assertRaises(BottleError):
            self.create_with(fake, ["tools"])
        self.assertIn("bottle/base:with-tools", fake.built_images)


class StartStopTest(BottleTestCase):
    def test_stop_stops_the_vm_and_egress(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "base")
        bottles.stop("gradle")
        self.assertEqual(fake.containers[bottle.container], "stopped")
        self.assertEqual(fake.egress, set())
        self.assertEqual(bottles.get("gradle").status, "ready")  # kept, just stopped

    def test_stop_a_stopped_bottle(self) -> None:
        fake = self.fake()
        bottles.create("gradle", "base")
        bottles.stop("gradle")
        fake.calls.clear()
        bottles.stop("gradle")
        self.assertEqual(fake.calls, [])

    def test_start_after_stop(self) -> None:
        fake = self.fake()
        bottle = bottles.create("gradle", "base")
        bottles.stop("gradle")
        bottles.start("gradle")
        self.assertEqual(fake.containers[bottle.container], "running")
        self.assertEqual(fake.egress, {"gradle"})

    def test_shutdown_stops_running_bottles_then_bottled(self) -> None:
        fake = self.fake()
        a = bottles.create("gradle", "base")
        b = bottles.create("gradle", "base")
        bottles.stop(b.name)
        fake.calls.clear()

        stopped, daemon_was_running = bottles.shutdown()

        self.assertEqual((stopped, daemon_was_running), (["gradle"], True))
        self.assertEqual(fake.containers, {a.container: "stopped", b.container: "stopped"})
        self.assertEqual(fake.calls, ["container_stop", "daemon_stop"])


class WorkspaceTestCase(BottleTestCase):
    """A local repo stands in for the bottle's /workspace, with real git run against it."""

    def setUp(self) -> None:
        super().setUp()
        self.fake_runtime = self.fake()
        self.bottle = bottles.create("gradle", "base")
        self.workspace = self.tmp / "workspace"
        run("git", "clone", "-q", self.repo_path, self.workspace)

        def in_workspace(argv):
            return [str(self.workspace) if a == "/workspace" else a for a in argv]

        def container_exec(name, argv, user=None, workdir=None):
            result = __import__("subprocess").run(in_workspace(argv), capture_output=True, text=True)
            if result.returncode != 0:
                raise BottleError(result.stderr)
            return result.stdout

        for patcher in (
            mock.patch.object(bottles.runtime, "exec_command", side_effect=lambda name, argv, user=None: in_workspace(argv)),
            mock.patch.object(bottles.runtime, "container_exec", side_effect=container_exec),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def host(self, *args: str) -> str:
        return run("git", "-C", self.repo_path, *args)

    def agent_commit(self, message: str) -> str:
        return self.commit(self.workspace, message)

    def ref(self, name: str) -> str:
        return self.host("rev-parse", f"refs/namespaces/bottle-gradle/refs/heads/{name}")

    def agent_push(self, branch: str = "main") -> None:
        """What `git push host` does in a bottle: an ordinary push, which the host
        puts in the bottle's namespace by running receive-pack with GIT_NAMESPACE."""
        namespace = bottles.ref_namespace(self.bottle)
        receive_pack = f"git --namespace={namespace} -c receive.denyCurrentBranch=ignore receive-pack"
        run("git", "-C", self.workspace, "push", "-q", "--receive-pack", receive_pack,
            str(self.repo_path), f"HEAD:refs/heads/{branch}")


class WorkspaceSetupTest(WorkspaceTestCase):
    def test_the_workspace_has_both_remotes_and_no_push_refspec(self) -> None:
        script = self.fake_runtime.workspace_script
        self.assertIn("remote add host http://bottle.host/git/host", script)
        self.assertIn("remote add origin http://bottle.host/git/origin", script)
        # The host puts receive-pack in the bottle's namespace, so pushing needs
        # no configuration here and git behaves as git.
        self.assertNotIn("remote.host.push", script)

    def test_the_context_tells_the_agent_where_its_work_goes(self) -> None:
        context = bottles.context(self.bottle)
        self.assertIn("`git push host`", context)
        self.assertIn("bottle-gradle", context)
        self.assertNotIn("agent/", context)


class ExecTest(BottleTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.fake_runtime = self.fake()
        bottles.create("gradle")
        self.attached: dict = {}
        patcher = mock.patch.object(
            bottles.runtime, "container_exec_interactive",
            side_effect=lambda name, argv, user=None, workdir=None, env=None, tty=True:
                self.attached.update(name=name, argv=argv, user=user, workdir=workdir, tty=tty))
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_runs_the_command_in_the_workspace(self) -> None:
        bottles.exec_("gradle", ["git", "push", "host"])
        self.assertEqual(self.attached["argv"], ["git", "push", "host"])
        self.assertEqual((self.attached["user"], self.attached["workdir"]), ("genie", "/workspace"))

    def test_a_tty_only_when_this_process_has_one(self) -> None:
        for isatty, expected in ((True, True), (False, False)):
            with self.subTest(isatty=isatty), mock.patch("sys.stdin.isatty", return_value=isatty), \
                    mock.patch("sys.stdout.isatty", return_value=isatty):
                bottles.exec_("gradle", ["true"])
                self.assertEqual(self.attached["tty"], expected)

    def test_shell_always_gets_a_tty(self) -> None:
        with mock.patch("sys.stdin.isatty", return_value=False):
            bottles.shell("gradle")
        self.assertEqual((self.attached["argv"], self.attached["tty"]), (["bash", "-l"], True))

    def test_no_command_is_an_error(self) -> None:
        with self.assertRaisesRegex(BottleError, "bottle exec needs a command"):
            bottles.exec_("gradle", [])

    def test_starts_a_stopped_bottle(self) -> None:
        bottles.stop("gradle")
        bottles.exec_("gradle", ["true"])
        self.assertIn("container_start", self.fake_runtime.calls)


class DeleteGuardTest(WorkspaceTestCase):
    """delete refuses to lose work the wrapped repo doesn't have, unless forced."""

    def test_clean_bottle_deletes(self) -> None:
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})

    def test_unpushed_commits_block_delete(self) -> None:
        run("git", "-C", self.workspace, "switch", "-q", "-c", "work")
        self.agent_commit("unpushed")
        with self.assertRaisesRegex(BottleError, r"has work its repo doesn't: branch work\. To keep it, push it from the bottle \(`git push host`\), or `bottle exec gradle git push host`; or delete anyway with --force"):
            bottles.delete("gradle")
        self.assertIn("gradle", bottles.load())

    def test_pushing_unblocks_delete(self) -> None:
        self.agent_commit("work")
        self.agent_push()
        bottles.delete("gradle")
        self.assertEqual(bottles.load(), {})

    def test_detached_head_and_uncommitted_changes_block_delete(self) -> None:
        run("git", "-C", self.workspace, "checkout", "-q", "--detach")
        self.agent_commit("detached")
        (self.workspace / "scratch.txt").write_text("wip")
        with self.assertRaisesRegex(BottleError, "detached HEAD, uncommitted changes. To keep it, commit the changes in the bottle and push it"):
            bottles.delete("gradle")

    def test_force_deletes_anyway(self) -> None:
        self.agent_commit("unpushed")
        bottles.delete("gradle", force=True)
        self.assertEqual(bottles.load(), {})

    def test_unable_to_check_needs_force(self) -> None:
        with mock.patch.object(bottles, "unsaved_work", side_effect=BottleError("exec failed")):
            with self.assertRaisesRegex(BottleError, "couldn't check gradle for unsaved work .*--force"):
                bottles.delete("gradle")


class ListTest(BottleTestCase):
    def test_states(self) -> None:
        fake = self.fake()
        a = bottles.create("gradle", "base")
        b = bottles.create("gradle", "base")
        fake.containers[b.container] = "stopped"
        self.assertEqual([(x.name, state) for x, state in bottles.list_all()], [("gradle", "running"), ("gradle-2", "stopped")])
        fake.containers.pop(a.container)
        self.assertEqual(bottles.list_all()[0][1], "missing")


class RepoBranchTest(GitTestCase):
    def test_default_start_is_origins_default_branch(self) -> None:
        origin = self.make_repo("origin")
        run("git", "-C", origin, "branch", "-m", "main", "trunk")
        clone = self.tmp / "clone"
        run("git", "clone", "-q", origin, clone)
        run("git", "-C", clone, "switch", "-q", "-c", "elsewhere")
        start = repos.default_start(repos.Repo("clone", clone))
        self.assertEqual(start, repos.Start("trunk", run("git", "-C", origin, "rev-parse", "trunk")))

    def test_default_start_without_origin_is_the_checked_out_branch(self) -> None:
        path = self.make_repo("r")
        run("git", "-C", path, "switch", "-q", "-c", "feature")
        head = self.commit(path, "on feature")
        self.assertEqual(repos.default_start(repos.Repo("r", path)), repos.Start("feature", head))

    def test_default_start_without_origin_on_a_detached_head(self) -> None:
        path = self.make_repo("r")
        first = run("git", "-C", path, "rev-parse", "HEAD")
        self.commit(path, "later")
        run("git", "-C", path, "checkout", "-q", "--detach", first)
        self.assertEqual(repos.default_start(repos.Repo("r", path)), repos.Start(None, first))

    def test_default_start_of_an_empty_repo(self) -> None:
        path = self.tmp / "empty"
        run("git", "init", "-q", path)
        with self.assertRaisesRegex(BottleError, "has no commits"):
            repos.default_start(repos.Repo("empty", path))

    def test_resolve_prefers_local_then_origin(self) -> None:
        origin = self.make_repo("origin")
        run("git", "-C", origin, "branch", "remote-only")
        clone = self.tmp / "clone"
        run("git", "clone", "-q", origin, clone)
        repo = repos.Repo("clone", clone)
        self.assertEqual(repos.resolve_branch(repo, "remote-only"), run("git", "-C", origin, "rev-parse", "remote-only"))
        local = self.commit(clone, "local work")
        self.assertEqual(repos.resolve_branch(repo, "main"), local)

    def test_objects_dir_of_a_linked_worktree_is_the_main_repos(self) -> None:
        path = self.make_repo("main-repo")
        worktree = self.tmp / "wt"
        run("git", "-C", path, "worktree", "add", "-q", worktree)
        self.assertEqual(repos.objects_dir(repos.Repo("wt", worktree)), path / ".git/objects")


class ResetTest(WorkspaceTestCase):
    def test_recreates_the_container_and_workspace(self) -> None:
        fake_run = mock.patch.object(bottles, "_run_container")
        with fake_run as run_container:
            bottles.reset("gradle")
        run_container.assert_called_once()
        self.assertEqual(bottles.get("gradle").status, "ready")

    def test_refuses_to_lose_work(self) -> None:
        self.agent_commit("unpushed")
        with self.assertRaisesRegex(BottleError, "has work its repo doesn't: branch main.*reset anyway with --force"):
            bottles.reset("gradle")
        with mock.patch.object(bottles, "_run_container"):
            bottles.reset("gradle", force=True)

    def test_a_stopped_bottle_comes_back_running(self) -> None:
        bottle = bottles.get("gradle")
        bottles.stop("gradle")
        # Checking for unsaved work starts the bottle; the contract check would run on this machine.
        with mock.patch.object(bottles, "verify_contract"), \
                mock.patch.object(bottles, "_run_container", side_effect=lambda b, r, t: self.fake_runtime.containers.__setitem__(b.container, "running")):
            bottles.reset("gradle")
        self.assertEqual(bottles.runtime.container_state(bottle.container), "running")

    def test_gets_the_repos_current_default_features_and_keeps_its_own(self) -> None:
        with mock.patch.object(bottles, "_run_container"), mock.patch.object(bottles.features_, "ensure_built"):
            bottles.delete("gradle", force=True)
            bottles.create("gradle", features=["jvm"])
            bottles.repos.set_settings("gradle", ["claude"])
            reset = bottles.reset("gradle")
        self.assertEqual(set(reset.features), {"claude", "jvm"})
        self.assertEqual(bottles.get("gradle").features, reset.features)

    def test_moves_to_the_latest_commit_of_its_branch(self) -> None:
        bottle = bottles.get("gradle")
        latest = self.commit(self.repo_path, "new host work")
        with mock.patch.object(bottles, "_run_container") as run_container:
            reset = bottles.reset("gradle")
        self.assertEqual(reset.commit, latest)
        self.assertEqual(bottles.get("gradle").commit, latest)
        self.assertEqual(run_container.call_args.args[0].commit, latest)

    def test_a_requested_branch_gone_from_the_repo_fails_before_anything_changes(self) -> None:
        with mock.patch.object(bottles, "_run_container", side_effect=lambda b, r, t: self.fake_runtime.containers.__setitem__(b.container, "running")):
            bottles.delete("gradle", force=True)
            bottles.create("gradle", branch="main")
        run("git", "-C", self.repo_path, "switch", "-q", "-c", "other")
        run("git", "-C", self.repo_path, "branch", "-D", "main")
        with self.assertRaisesRegex(BottleError, "has no branch 'main'"):
            bottles.reset("gradle")
        self.assertIn(bottles.get("gradle").container, self.fake_runtime.containers)

    def test_without_a_requested_branch_it_follows_the_repos_default(self) -> None:
        run("git", "-C", self.repo_path, "switch", "-q", "-c", "other")
        run("git", "-C", self.repo_path, "branch", "-D", "main")
        with mock.patch.object(bottles, "_run_container"):
            self.assertEqual(bottles.reset("gradle", force=True).branch, "other")

    def test_repairs_a_missing_container(self) -> None:
        bottle = bottles.get("gradle")
        del self.fake_runtime.containers[bottle.container]
        with mock.patch.object(bottles, "_run_container", side_effect=lambda b, r, t: self.fake_runtime.containers.__setitem__(b.container, "running")):
            bottles.reset("gradle")  # no work check: nothing to check
        self.assertEqual(bottles.runtime.container_state(bottle.container), "running")


if __name__ == "__main__":
    unittest.main()
