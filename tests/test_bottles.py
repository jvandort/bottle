import json
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from bottle import bottles, features as features_, host, repos
from bottle.errors import BottleError
from tests.support import GitTestCase, run


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
        self.bridged = True
        self.anchored: set[str] = set()  # the networks the anchor is on
        self.services = True  # container's system services; down after the machine restarts

    def services_running(self):
        return self.services

    def _visible(self) -> None:
        """As runtime's inspects do: start the services if they're down, rather than report everything gone."""
        if not self.services:
            self.calls.append("start_services")
            self.services = True

    def _step(self, name: str) -> None:
        self.calls.append(name)
        if self.fail == name:
            raise BottleError(f"{name} failed")

    def network_create(self, network, labels=None):
        self.statuses_seen.append(bottles.load()[next(iter(bottles.load()))].status)
        self._step("network_create")
        self.networks.add(network)

    gateway = "192.168.128.1"

    def network_gateway(self, network):
        return self.gateway

    def host_has_address(self, address):
        return self.bridged

    def network_delete(self, network):
        if self.fail_cleanup == "network":
            raise BottleError("network busy")
        if network in self.anchored:
            raise BottleError(f"cannot delete subnet {network} with referring containers")
        self.networks.discard(network)

    def container_run(self, name, image, network, env, mounts, labels=None, memory=None):
        self._step("container_run")
        self.run_args = dict(name=name, image=image, network=network, env=env, mounts=mounts, labels=labels, memory=memory)
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
        self._visible()
        return network in self.networks

    def container_state(self, name):
        self._visible()
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
        self._visible()
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
    standins: dict = {}
    intercepted: list = ["ci.test"]

    def container_exec(self, name, argv, user=None, workdir=None, input=None, env=None):
        if "/etc/environment" in argv[-1]:
            self._step("point_at_egress")
            self.environment = (user, input)
            return ""
        if "# bottle contract" in argv[-1]:
            self._step("verify_contract")
            return self.contract_output
        if bottles.CA_CERTIFICATE in argv[-1]:
            self._step("trust_egress_ca")
            self.trusted = (user, input)
            return ""
        self._step("init_workspace")
        self.workspace_script = argv[-1]
        return ""

    def ensure_egress(self, bottle, network, git_dir=None, features=(), remotes=None):
        self._step("ensure_egress")
        self.egress_git_dir = git_dir
        self.egress_features = tuple(features)
        self.egress_remotes = remotes
        self.egress.add(bottle)
        return "http://192.168.128.1:3128"

    def release_egress(self, bottle):
        self.egress.discard(bottle)

    def ensure_anchor(self, networks):
        self._step("anchor")
        self.anchored = set(networks) & self.networks

    def remove_anchor(self):
        self.calls.append("anchor_remove")
        self.anchored = set()

    def patch(self, test: unittest.TestCase) -> None:
        for module, names in (
            (bottles.runtime, ("network_create", "network_gateway", "host_has_address", "network_delete", "network_exists",
                               "container_run", "container_state", "container_start", "container_stop",
                               "container_delete", "container_info", "container_exec", "images", "images_in_use",
                               "image_delete", "services_running")),
            (bottles.daemon, ("ensure_egress", "release_egress", "stop")),
        ):
            for name in names:
                patcher = mock.patch.object(module, name, getattr(self, "stop_daemon" if name == "stop" else name))
                patcher.start()
                test.addCleanup(patcher.stop)
        for patcher in (
            mock.patch.object(bottles.prereqs, "ensure_container"),
            mock.patch.object(bottles.anchor, "ensure", self.ensure_anchor),
            mock.patch.object(bottles.anchor, "remove", self.remove_anchor),
            mock.patch.object(bottles.ca, "certificate", return_value="the egress CA's certificate"),
            mock.patch.object(bottles.auth, "intercepted_hosts", side_effect=lambda specs: list(self.intercepted)),
            mock.patch.object(bottles.images, "is_current", return_value=True),
            mock.patch.object(bottles.features_, "remove_stale", return_value=[]),
            mock.patch.object(bottles.auth, "standins_for", side_effect=lambda specs: dict(self.standins)),
            mock.patch.object(bottles.auth, "ensure_logged_in", side_effect=lambda specs: self.calls.append("ensure_logged_in")),
        ):
            patcher.start()
            test.addCleanup(patcher.stop)


class BottleTestCase(GitTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.repo_path = self.make_repo("example")
        self.head = run("git", "-C", self.repo_path, "rev-parse", "HEAD")

    def create(self, image: str = "base", branch: str | None = None, name: str | None = None,
               features=(), memory: str | None = None, context: str = "", remotes=None) -> bottles.Bottle:
        """A bottle with the example repo checked out: named example, then example-2, ..., unless named."""
        taken = set(bottles.load())
        name = name or next(n for n in ("example", *(f"example-{i}" for i in range(2, 100))) if n not in taken)
        spec = {"features": list(features), **({"memory": memory} if memory else {})}
        return bottles.create(spec, name, self.repo_path, remotes, branch, context, image)

    def fake(self, **kwargs) -> FakeRuntime:
        fake = FakeRuntime(**kwargs)
        fake.patch(self)
        return fake


class CreateTest(BottleTestCase):
    def test_creates_every_part(self) -> None:
        fake = self.fake()

        bottle = self.create()

        self.assertEqual((bottle.name, bottle.branch, bottle.commit, bottle.status), ("example", "main", self.head, "ready"))
        self.assertEqual(bottles.get("example"), bottle)
        self.assertEqual(fake.networks, {bottle.network})
        self.assertEqual(fake.containers, {bottle.container: "running"})
        self.assertEqual(fake.egress, {"example"})

    def test_egress_serves_the_repo_as_origin(self) -> None:
        fake = self.fake()
        self.create()
        self.assertEqual(fake.egress_git_dir, self.repo_path / ".git")
        self.assertEqual(fake.egress_remotes, (host.Remote("origin", host.UPSTREAM), host.Remote("host", host.HEADS)))
        self.assertIn("remote add origin http://bottle.host/git/origin", fake.workspace_script)
        self.assertIn("remote add host http://bottle.host/git/host", fake.workspace_script)
        self.assertIn("fetch -q --multiple origin host", fake.workspace_script)

    def test_order_container_before_egress(self) -> None:
        fake = self.fake()
        self.create()
        self.assertEqual(fake.calls, ["ensure_logged_in", "network_create", "anchor", "container_run", "verify_contract", "trust_egress_ca", "ensure_egress", "init_workspace"])

    def test_record_is_written_before_anything_is_created(self) -> None:
        fake = self.fake()
        self.create()
        self.assertEqual(fake.statuses_seen, ["creating"])

    def test_container_gets_image_proxy_and_read_only_objects(self) -> None:
        fake = self.fake()
        self.create()
        self.assertEqual(fake.run_args["image"], "bottle/base:latest")
        self.assertEqual(fake.run_args["env"]["HTTPS_PROXY"], "http://192.168.128.1:3128")
        self.assertEqual(fake.run_args["env"]["http_proxy"], "http://192.168.128.1:3128")
        [mount] = fake.run_args["mounts"]
        self.assertEqual((mount.source, mount.target, mount.readonly), (self.repo_path / ".git/objects", "/mnt/repo/objects", True))

    def test_workspace_setup_writes_the_agent_context(self) -> None:
        fake = self.fake()
        bottle = self.create()
        self.assertIn('> "$HOME/BOTTLE.md"', fake.workspace_script)
        context = bottles.context(bottle)
        self.assertIn("checkout of `example`, on the `main` branch.", context)
        self.assertNotIn(self.head[:12], context)  # no commits: it never goes stale
        self.assertIn("You're `genie`, running in a bottle", context)
        self.assertIn("passwordless sudo", context)
        self.assertIn("only through the HTTP proxy", context)

    def test_context_for_a_detached_commit(self) -> None:
        bottle = bottles.Bottle("b", "id", "base", None, "c" * 40, 0.0, git_repo="/src/r")
        self.assertIn("checkout of `r`, at a detached commit.", bottles.context(bottle))

    def test_context_has_what_each_feature_says(self) -> None:
        bottle = bottles.Bottle("b", "id", "base", None, "c" * 40, 0.0, features=("jvm:version=21", "tools"))
        context = bottles.context(bottle)
        self.assertIn("\n## Features\n\n### Feature 'jvm'\n\nTemurin JDK 21 is the default `java`", context)
        self.assertIn("\n### Feature 'tools'\n\n", context)
        self.assertNotIn("## Features", bottles.context(bottles.Bottle("b", "id", "base", None, "c" * 40, 0.0)))

    def test_workspace_script_quotes_values(self) -> None:
        run("git", "-C", self.repo_path, "branch", "we$rd;branch")
        fake = self.fake()
        self.create(branch="we$rd;branch")
        self.assertIn("git init -q -b 'we$rd;branch' /workspace", fake.workspace_script)

    def test_detached_start(self) -> None:
        self.commit(self.repo_path, "later")
        run("git", "-C", self.repo_path, "checkout", "-q", "--detach", self.head)
        fake = self.fake()
        bottle = self.create()
        self.assertEqual((bottle.branch, bottle.commit, bottle.checkout), (None, self.head, f"({self.head[:12]})"))
        self.assertIn(f"update-ref --no-deref HEAD {self.head}", fake.workspace_script)
        self.assertEqual(bottles.get("example"), bottle)  # survives the JSON round trip

    def test_explicit_name_and_branch(self) -> None:
        run("git", "-C", self.repo_path, "branch", "feature")
        self.fake()
        bottle = self.create(branch="feature", name="mine")
        self.assertEqual((bottle.name, bottle.branch), ("mine", "feature"))

    def test_duplicate_name(self) -> None:
        self.fake()
        self.create(name="mine")
        with self.assertRaisesRegex(BottleError, "already exists"):
            self.create(name="mine")

    def test_unknown_image(self) -> None:
        self.fake()
        with self.assertRaisesRegex(BottleError, "no image named 'nope'"):
            self.create(image="nope")
        self.assertEqual(bottles.load(), {})

    def test_builds_the_image_first(self) -> None:
        fake = self.fake()

        def ensure_built(image, feature_ids):
            fake.calls.append(f"ensure_built {image} {feature_ids}")
            return "bottle/base:with-tools"

        with mock.patch.object(bottles.features_, "ensure_built", side_effect=ensure_built):
            bottle = self.create(features=["tools"])
        self.assertEqual(fake.calls[0], "ensure_built base ['tools']")
        self.assertEqual(fake.run_args["image"], "bottle/base:with-tools")
        self.assertEqual((bottle.features, bottle.environment), (("tools",), "base+tools"))
        self.assertEqual(bottles.get("example").features, ("tools",))  # survives the JSON round trip

    def test_records_feature_dependencies(self) -> None:
        self.fake()
        resolved = [SimpleNamespace(id="tools", spec="tools", agent_context=str),
                    SimpleNamespace(id="jvm", spec="jvm", agent_context=str)]
        with mock.patch.object(bottles.features_, "resolve", return_value=resolved), \
                mock.patch.object(bottles.features_, "ensure_built", return_value="bottle/base:with-jvm.tools"):
            bottle = self.create(features=["jvm"])
        self.assertEqual(bottle.features, ("tools", "jvm"))
        self.assertEqual(bottle.environment, "base+jvm+tools")

    def test_old_records_without_features_load(self) -> None:
        self.fake()
        self.create()
        data = json.loads(bottles.registry_path().read_text())
        del data["bottles"]["example"]["features"]
        bottles.registry_path().write_text(json.dumps(data))
        self.assertEqual(bottles.get("example").features, ())

    def test_unknown_branch(self) -> None:
        self.fake()
        with self.assertRaisesRegex(BottleError, "has no branch or commit 'nope'"):
            self.create(branch="nope")


class RollbackTest(BottleTestCase):
    def test_failure_at_each_step_leaves_nothing(self) -> None:
        for step in ("network_create", "container_run", "verify_contract", "trust_egress_ca", "ensure_egress", "init_workspace"):
            with self.subTest(step):
                fake = self.fake(fail=step)
                with self.assertRaisesRegex(BottleError, f"{step} failed"):
                    self.create()
                self.assertEqual(bottles.load(), {})
                self.assertEqual((fake.networks, fake.containers, fake.egress), (set(), {}, set()))

    def test_interrupt_rolls_back(self) -> None:
        fake = self.fake()
        with mock.patch.object(bottles.runtime, "container_run", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                self.create()
        self.assertEqual(bottles.load(), {})
        self.assertEqual(fake.networks, set())

    def test_failed_cleanup_keeps_a_broken_record(self) -> None:
        self.fake(fail="init_workspace", fail_cleanup="network")
        with self.assertRaisesRegex(BottleError, "init_workspace failed; cleanup also failed: .*network busy"):
            self.create()
        self.assertEqual(bottles.get("example").status, "broken")


class DeleteTest(BottleTestCase):
    def test_removes_every_part(self) -> None:
        fake = self.fake()
        self.create()
        bottles.delete("example")
        self.assertEqual(bottles.load(), {})
        self.assertEqual((fake.networks, fake.containers, fake.egress), (set(), {}, set()))

    def test_finishes_off_a_half_made_bottle(self) -> None:
        fake = self.fake()
        bottle = self.create()
        # As if creation died after the network: no container, no egress.
        fake.containers.clear()
        fake.egress.clear()
        bottles.delete("example")
        self.assertEqual(bottles.load(), {})
        self.assertNotIn(bottle.network, fake.networks)

    def test_retry_after_a_failed_delete(self) -> None:
        fake = self.fake(fail_cleanup="network")
        self.create()
        with self.assertRaisesRegex(BottleError, "rerun `bottle delete example`"):
            bottles.delete("example")
        self.assertEqual(bottles.get("example").status, "broken")
        self.assertEqual(fake.containers, {})  # later steps still ran

        fake.fail_cleanup = None
        bottles.delete("example")
        self.assertEqual(bottles.load(), {})

    def test_unknown_bottle(self) -> None:
        with self.assertRaisesRegex(BottleError, "no bottle named 'nope'"):
            bottles.delete("nope")

    def test_after_a_restart_starts_the_services_and_removes_every_part(self) -> None:
        fake = self.fake()
        bottle = self.create()
        fake.containers[bottle.container] = "stopped"
        fake.services = False  # the machine restarted
        bottles.delete("example")
        self.assertEqual(bottles.load(), {})
        self.assertEqual((fake.networks, fake.containers), (set(), {}))
        self.assertIn("start_services", fake.calls)


class AnchorTest(BottleTestCase):
    def test_every_bottles_network_is_anchored(self) -> None:
        fake = self.fake()
        one, two = self.create(), self.create()
        self.assertEqual(fake.anchored, {one.network, two.network})

    def test_delete_takes_the_anchor_off_the_network_first(self) -> None:
        fake = self.fake()
        one, two = self.create(), self.create()
        bottles.delete(one.name)
        self.assertEqual((fake.anchored, fake.networks), ({two.network}, {two.network}))

    def test_rollback_takes_the_anchor_off_the_network(self) -> None:
        fake = self.fake(fail="init_workspace")
        with self.assertRaisesRegex(BottleError, "init_workspace failed"):
            self.create()
        self.assertEqual((fake.anchored, fake.networks), (set(), set()))

    def test_starting_a_bottle_anchors_first(self) -> None:
        fake = self.fake()
        bottle = self.create()
        fake.containers[bottle.container] = "stopped"
        fake.anchored = set()  # as after the machine restarts
        fake.calls.clear()
        bottles.ensure_running("example")
        self.assertEqual(fake.calls[:2], ["anchor", "container_start"])
        self.assertEqual(fake.anchored, {bottle.network})


class EnsureRunningTest(BottleTestCase):
    def test_starts_a_stopped_bottle_and_its_egress(self) -> None:
        fake = self.fake()
        bottle = self.create()
        fake.containers[bottle.container] = "stopped"
        fake.egress.clear()

        bottles.ensure_running("example")

        self.assertEqual(fake.containers[bottle.container], "running")
        self.assertEqual(fake.egress, {"example"})

    def test_starting_verifies_the_contract(self) -> None:
        fake = self.fake()
        bottle = self.create()
        fake.containers[bottle.container] = "stopped"
        fake.calls.clear()
        bottles.ensure_running("example")
        self.assertEqual(fake.calls, ["anchor", "container_start", "verify_contract", "trust_egress_ca", "point_at_egress", "ensure_egress"])

    def test_starting_points_ssh_sessions_at_the_current_proxy(self) -> None:
        fake = self.fake()
        bottle = self.create()
        fake.containers[bottle.container] = "stopped"
        fake.gateway = "192.168.130.1"
        bottles.ensure_running("example")
        user, environment = fake.environment
        self.assertEqual(user, "root")
        self.assertIn("HTTPS_PROXY=http://192.168.130.1:3128\n", environment)
        self.assertIn("NO_PROXY=localhost,127.0.0.1\n", environment)

    def test_running_bottle_only_ensures_egress(self) -> None:
        fake = self.fake()
        self.create()
        fake.calls.clear()
        bottles.ensure_running("example")
        self.assertEqual(fake.calls, ["anchor", "ensure_egress"])

    def test_starting_installs_the_egress_ca_as_root(self) -> None:
        fake = self.fake()
        bottle = self.create()
        fake.containers[bottle.container] = "stopped"
        fake.trusted = None
        bottles.ensure_running("example")
        self.assertEqual(fake.trusted, ("root", "the egress CA's certificate"))

    def test_running_bottle_without_its_bridge_is_left_alone(self) -> None:
        # Restarting it would take its bridge back from whichever network has it now.
        fake = self.fake()
        self.create()
        fake.bridged = False
        fake.calls.clear()
        with self.assertRaisesRegex(BottleError, "lost its bridge .* `bottle shutdown && container system stop`"):
            bottles.ensure_running("example")
        self.assertEqual(fake.calls, [])

    def test_after_a_restart_starts_the_services_then_the_bottle(self) -> None:
        fake = self.fake()
        bottle = self.create()
        fake.containers[bottle.container] = "stopped"
        fake.services = False
        bottles.ensure_running("example")
        self.assertEqual(fake.containers[bottle.container], "running")

    def test_missing_container(self) -> None:
        fake = self.fake()
        self.create()
        fake.containers.clear()
        with self.assertRaisesRegex(BottleError, "container is gone; `bottle delete example`, then `bottle new` again"):
            bottles.ensure_running("example")

    def test_unfinished_bottle(self) -> None:
        self.fake(fail="init_workspace", fail_cleanup="network")
        with self.assertRaises(BottleError):
            self.create()
        with self.assertRaisesRegex(BottleError, "example is broken"):
            bottles.ensure_running("example")


class ContractTest(BottleTestCase):
    def test_every_check_has_a_description(self) -> None:
        self.assertTrue(all(what and test for what, test in bottles.CONTRACT))

    def test_a_broken_contract_fails_creation_and_rolls_back(self) -> None:
        fake = self.fake()
        fake.contract_output = "git is installed\n/workspace exists and genie can write to it\n"
        with self.assertRaisesRegex(
            BottleError, "base doesn't meet the bottle contract: git is installed; /workspace exists and genie can write to it"
        ):
            self.create()
        self.assertEqual((bottles.load(), fake.containers), ({}, {}))

    def test_image_defaults_to_base(self) -> None:
        fake = self.fake()
        self.assertEqual(self.create().image, "base")
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
        # Which of them fail depends on where the suite runs -- the host machine fails most, a bottle
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


class StandinTest(BottleTestCase):
    """A bottle gets a fake token, so a tool believes it's logged in; the proxy holds the real one."""

    def test_a_bottle_with_the_claude_feature_gets_one(self) -> None:
        env = bottles._container_env("http://192.168.128.1:3128", ("claude",))
        self.assertEqual(env["CLAUDE_CODE_OAUTH_TOKEN"], features_.STANDIN)
        # The entrypoint mirrors it into /etc/environment, for SSH sessions.
        self.assertEqual(env["BOTTLE_MIRROR_ENV"], "CLAUDE_CODE_OAUTH_TOKEN")

    def test_features_that_ask_for_none_add_nothing(self) -> None:
        proxy = "http://192.168.128.1:3128"
        self.assertEqual(bottles._container_env(proxy, ("tools",)), bottles._proxy_env(proxy))

    def test_the_bottle_a_login_runs_in_gets_none(self) -> None:
        """`bottle auth login claude` must find nothing that looks like a token."""
        fake = self.fake()
        self.create()  # the fake runtime reports on a bottle that exists
        with mock.patch.object(bottles.features_, "ensure_built", return_value="bottle/base:with-claude"), \
                bottles.throwaway(["claude"]):
            pass
        self.assertEqual(fake.run_args["env"], bottles._proxy_env("http://192.168.128.1:3128"))
        # Nor does its egress attach one: a login mustn't send the credential it's replacing.
        self.assertEqual(fake.egress_features, ())
        # And it trusts no egress CA, since its proxy terminates nothing.
        self.assertEqual(fake.calls[-2:], ["container_run", "ensure_egress"])


class CredentialTest(BottleTestCase):
    def test_new_start_and_shell_log_in_first(self) -> None:
        fake = self.fake()
        self.create()
        self.assertEqual(fake.calls[0], "ensure_logged_in")
        fake.calls.clear()
        bottles.start("example")
        self.assertEqual(fake.calls[0], "ensure_logged_in")
        fake.calls.clear()
        with mock.patch.object(bottles.runtime, "container_exec_interactive"):
            bottles.shell("example")
        self.assertEqual(fake.calls[0], "ensure_logged_in")

    def test_shell_hands_the_bottle_nothing(self) -> None:
        """Credentials live at the egress proxy, so a session gets none of them: only where the proxy is."""
        self.fake()
        self.create()
        with mock.patch.object(bottles.runtime, "container_exec_interactive") as interactive:
            bottles.shell("example")
        self.assertEqual(interactive.call_args.kwargs["env"], bottles._proxy_env("http://192.168.128.1:3128"))

    def test_shell_follows_the_proxy_when_the_gateway_moves(self) -> None:
        fake = self.fake()
        self.create()
        fake.gateway = "192.168.130.1"  # container's services restarted and gave the network another subnet
        with mock.patch.object(bottles.runtime, "container_exec_interactive") as interactive:
            bottles.shell("example")
        self.assertEqual(interactive.call_args.kwargs["env"]["HTTPS_PROXY"], "http://192.168.130.1:3128")


class OwnershipTest(BottleTestCase):
    """Bottle must never touch a container it didn't make (a scratch home's rollback once did)."""

    def someone_elses(self, fake, name: str) -> None:
        fake.containers[name] = "running"
        fake.container_labels[name] = {"bottle.id": "someone-else"}
        fake.container_networks[name] = "their-network"

    def test_new_refuses_a_taken_name_and_leaves_it_alone(self) -> None:
        fake = self.fake()
        self.someone_elses(fake, "bottle-" + bottles.namespace() + "example")
        with self.assertRaisesRegex(BottleError, "already exists; choose another name"):
            self.create()
        self.assertEqual(fake.containers, {"bottle-" + bottles.namespace() + "example": "running"})
        self.assertEqual(bottles.load(), {})
        self.assertEqual(fake.calls, ["ensure_logged_in"])  # nothing was created, so nothing rolled back

    def test_delete_leaves_a_container_that_isnt_the_bottles(self) -> None:
        fake = self.fake()
        bottle = self.create()
        self.someone_elses(fake, bottle.container)  # replaced behind bottle's back
        with self.assertRaisesRegex(BottleError, "isn't this bottle's"):
            bottles.delete("example")
        self.assertIn(bottle.container, fake.containers)
        self.assertEqual(bottles.get("example").status, "broken")

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
        bottle = self.create()
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
                mock.patch.object(bottles.features_, "resolve", side_effect=lambda specs: [SimpleNamespace(id=s, spec=s, overrides=(), agent_context=str) for s in specs]), \
                mock.patch.object(bottles.features_, "image_tag", return_value=tag):
            return self.create(features=features, name=name), tag

    def test_delete_keeps_the_bottles_image_for_the_next_one(self) -> None:
        """A cache: the next bottle with those features starts without a build."""
        fake = self.fake()
        _, tag = self.create_with(fake, ["tools"])
        bottles.delete("example")
        self.assertIn(tag, fake.built_images)
        self.assertNotIn(f"image_delete {tag}", fake.calls)

    def test_delete_removes_stale_images(self) -> None:
        fake = self.fake()
        self.create_with(fake, ["tools"])
        with mock.patch.object(bottles.features_, "remove_stale", return_value=[]) as remove_stale:
            bottles.delete("example")
        remove_stale.assert_called_once_with()

    def test_keeps_the_base_image(self) -> None:
        fake = self.fake()
        self.create_with(fake, [])
        bottles.delete("example")
        self.assertIn("bottle/base:latest", fake.built_images)

    def test_rollback_keeps_the_freshly_built_image(self) -> None:
        fake = self.fake(fail="init_workspace")
        with self.assertRaises(BottleError):
            self.create_with(fake, ["tools"])
        self.assertIn("bottle/base:with-tools", fake.built_images)


class StartStopTest(BottleTestCase):
    def test_stop_stops_the_vm_and_egress(self) -> None:
        fake = self.fake()
        bottle = self.create()
        bottles.stop("example")
        self.assertEqual(fake.containers[bottle.container], "stopped")
        self.assertEqual(fake.egress, set())
        self.assertEqual(bottles.get("example").status, "ready")  # kept, just stopped

    def test_stop_a_stopped_bottle(self) -> None:
        fake = self.fake()
        self.create()
        bottles.stop("example")
        fake.calls.clear()
        bottles.stop("example")
        self.assertEqual(fake.calls, [])

    def test_stopping_doesnt_start_the_services_just_to_find_nothing_running(self) -> None:
        fake = self.fake()
        self.create()
        fake.services = False
        fake.calls.clear()
        bottles.stop("example")
        bottles.shutdown()
        self.assertEqual(fake.calls, ["daemon_stop"])

    def test_start_after_stop(self) -> None:
        fake = self.fake()
        bottle = self.create()
        bottles.stop("example")
        bottles.start("example")
        self.assertEqual(fake.containers[bottle.container], "running")
        self.assertEqual(fake.egress, {"example"})

    def test_shutdown_stops_running_bottles_then_the_anchor_then_bottled(self) -> None:
        fake = self.fake()
        a = self.create()
        b = self.create()
        bottles.stop(b.name)
        fake.calls.clear()

        stopped, daemon_was_running = bottles.shutdown()

        self.assertEqual((stopped, daemon_was_running), (["example"], True))
        self.assertEqual(fake.containers, {a.container: "stopped", b.container: "stopped"})
        self.assertEqual(fake.calls, ["container_stop", "anchor_remove", "daemon_stop"])


class ExecTest(BottleTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.fake_runtime = self.fake()
        self.create()
        self.attached: dict = {}
        patcher = mock.patch.object(
            bottles.runtime, "container_exec_interactive",
            side_effect=lambda name, argv, user=None, workdir=None, env=None, tty=True:
                self.attached.update(name=name, argv=argv, user=user, workdir=workdir, tty=tty))
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_runs_the_command_in_the_workspace(self) -> None:
        bottles.exec_("example", ["git", "push", "host"])
        self.assertEqual(self.attached["argv"], ["git", "push", "host"])
        self.assertEqual((self.attached["user"], self.attached["workdir"]), ("genie", "/workspace"))

    def test_a_tty_only_when_this_process_has_one(self) -> None:
        for isatty, expected in ((True, True), (False, False)):
            with self.subTest(isatty=isatty), mock.patch("sys.stdin.isatty", return_value=isatty), \
                    mock.patch("sys.stdout.isatty", return_value=isatty):
                bottles.exec_("example", ["true"])
                self.assertEqual(self.attached["tty"], expected)

    def test_shell_always_gets_a_tty(self) -> None:
        with mock.patch("sys.stdin.isatty", return_value=False):
            bottles.shell("example")
        self.assertEqual((self.attached["argv"], self.attached["tty"]), (["bash", "-l"], True))

    def test_no_command_is_an_error(self) -> None:
        with self.assertRaisesRegex(BottleError, "bottle exec needs a command"):
            bottles.exec_("example", [])

    def test_starts_a_stopped_bottle(self) -> None:
        bottles.stop("example")
        bottles.exec_("example", ["true"])
        self.assertIn("container_start", self.fake_runtime.calls)


class ListTest(BottleTestCase):
    def test_states(self) -> None:
        fake = self.fake()
        a = self.create()
        b = self.create()
        fake.containers[b.container] = "stopped"
        self.assertEqual([(x.name, state) for x, state in bottles.list_all()], [("example", "running"), ("example-2", "stopped")])
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


if __name__ == "__main__":
    unittest.main()


class SpecBottleTest(BottleTestCase):
    """`bottle new --spec`: features and memory from a spec, a repo and remotes only if given."""

    SPEC = {"features": [], "memory": "all"}

    def create(self, name: str = "t", **kwargs) -> bottles.Bottle:
        return bottles.create(dict(self.SPEC), name, **kwargs)

    def test_with_no_repo_there_is_nothing_to_mount_or_serve(self) -> None:
        fake = self.fake()
        bottle = self.create()
        self.assertEqual((bottle.name, bottle.git_repo, bottle.remotes, bottle.status), ("t", None, (), "ready"))
        self.assertEqual(fake.run_args["mounts"], [])
        self.assertIsNone(fake.egress_git_dir)
        self.assertNotIn("git init", fake.workspace_script)
        self.assertIn('> "$HOME/BOTTLE.md"', fake.workspace_script)
        self.assertIn("`/workspace` is empty: this bottle has no repo.", bottles.context(bottle))

    def test_a_repo_is_mounted_and_served_from_read_only_by_default(self) -> None:
        fake = self.fake()
        bottle = self.create(git_repo=self.repo_path)
        self.assertEqual((bottle.git_repo, bottle.branch, bottle.commit), (str(self.repo_path), "main", self.head))
        [mount] = fake.run_args["mounts"]
        self.assertEqual((mount.source, mount.target), (self.repo_path / ".git/objects", "/mnt/repo/objects"))
        self.assertEqual(fake.egress_git_dir, self.repo_path / ".git")
        self.assertEqual(fake.egress_remotes, (host.Remote("origin", host.UPSTREAM), host.Remote("host", host.HEADS)))
        self.assertIn("fetch -q --multiple origin host", fake.workspace_script)
        self.assertNotIn("branch.main.remote", fake.workspace_script)  # tracking is the creator's to set

    def test_serves_the_remotes_it_was_given(self) -> None:
        fake = self.fake()
        bottle = self.create(git_repo=self.repo_path, remotes=["up:fetch=upstream", "work:fetch=ns/t-in,push=ns/t-out"])
        self.assertEqual([r.spec for r in fake.egress_remotes], ["up:fetch=upstream", "work:fetch=ns/t-in,push=ns/t-out"])
        self.assertIn("remote add up http://bottle.host/git/up", fake.workspace_script)
        self.assertIn("fetch -q --multiple up work", fake.workspace_script)
        self.assertNotIn("checkout.defaultRemote", fake.workspace_script)  # no origin to default to
        self.assertEqual(bottles.get(bottle.name).remotes, bottle.remotes)  # survives the JSON round trip

    def test_registers_nothing_in_the_repo(self) -> None:
        self.fake()
        self.create(git_repo=self.repo_path, remotes=["work:fetch=ns/t-in,push=ns/t-out"])
        self.assertEqual(run("git", "-C", self.repo_path, "remote"), "")

    def test_checks_out_a_branch_or_a_commit(self) -> None:
        self.fake()
        run("git", "-C", self.repo_path, "branch", "feature")
        self.assertEqual(self.create("a", git_repo=self.repo_path, checkout="feature").branch, "feature")
        detached = self.create("b", git_repo=self.repo_path, checkout=self.head[:10])
        self.assertEqual((detached.branch, detached.commit), (None, self.head))
        with self.assertRaisesRegex(BottleError, "no branch or commit 'nope'"):
            self.create(git_repo=self.repo_path, checkout="nope")

    def test_memory_and_features_come_from_the_spec(self) -> None:
        fake = self.fake()
        with mock.patch.object(bottles.specs.runtime, "host_memory_mib", return_value=16384), \
                mock.patch.object(bottles.features_, "ensure_built", return_value="bottle/base:with-tools"):
            bottle = bottles.create({"features": ["tools"], "memory": "8G"}, "t")
        self.assertEqual((bottle.features, bottle.memory, fake.run_args["memory"]), (("tools",), "8G", "8G"))
        self.assertEqual(bottle.spec, {"features": ["tools"], "memory": "8G"})

    def test_refuses_remotes_without_a_repo(self) -> None:
        self.fake()
        for kwargs in ({"remotes": ["origin:fetch=upstream"]}, {"checkout": "main"}):
            with self.subTest(kwargs), self.assertRaisesRegex(BottleError, "need --git-repo"):
                self.create(**kwargs)

    def test_refuses_a_subdirectory_for_the_repo(self) -> None:
        self.fake()
        (self.repo_path / "sub").mkdir()
        with self.assertRaisesRegex(BottleError, "inside a git repo"):
            self.create(git_repo=self.repo_path / "sub")

    def test_refuses_a_remote_twice_or_a_namespace_with_two_writers(self) -> None:
        self.fake()
        for remotes, message in (
            (["a:fetch=heads", "a:fetch=upstream"], "remote a given twice"),
            (["a:fetch=heads,push=ns/x", "b:fetch=heads,push=ns/x"], "two remotes push into namespace x"),
        ):
            with self.subTest(remotes), self.assertRaisesRegex(BottleError, message):
                self.create(git_repo=self.repo_path, remotes=remotes)

    def test_each_namespace_has_one_writer_across_bottles(self) -> None:
        self.fake()
        self.create("a", git_repo=self.repo_path, remotes=["work:fetch=ns/t-in,push=ns/t-out"])
        with self.assertRaisesRegex(BottleError, "bottle a already pushes into namespace t-out"):
            self.create("b", git_repo=self.repo_path, remotes=["mine:fetch=heads,push=ns/t-out"])
        # Reading another bottle's namespace is fine; so is writing one in another repo.
        self.create("c", git_repo=self.repo_path, remotes=["peek:fetch=ns/t-out"])
        other = self.make_repo("other")
        self.create("d", git_repo=other, remotes=["work:fetch=ns/t-in,push=ns/t-out"])

    def test_a_taken_name_is_refused_not_numbered(self) -> None:
        self.fake()
        self.create("a")
        with self.assertRaisesRegex(BottleError, "a bottle named 'a' already exists"):
            self.create("a")

    def test_refuses_an_invalid_name(self) -> None:
        self.fake()
        for name in ("", "-a", "a/b", "a b"):
            with self.subTest(name), self.assertRaisesRegex(BottleError, "invalid name"):
                self.create(name)

    def test_context_is_facts_then_the_creators_own(self) -> None:
        self.fake()
        bottle = self.create(git_repo=self.repo_path, remotes=["origin:fetch=upstream", "work:fetch=ns/t-in,push=ns/t-out"],
                             context="# Thread\n\nWork on `t`.\n")
        context = bottles.context(bottle)
        self.assertIn("checkout of `example`, on the `main` branch.", context)
        self.assertIn("- `origin`: the repo's upstream, up to date whenever you fetch. Read-only.", context)
        self.assertIn("- `work`: refs the host keeps for this bottle. You can push to it", context)
        self.assertIn("Fetching it doesn't show your own pushes.", context)
        self.assertNotIn("Messy history", context)  # how to work is the creator's to say
        self.assertTrue(context.endswith("\n# Thread\n\nWork on `t`.\n"))

    def test_delete_doesnt_ask_about_unsaved_work(self) -> None:
        """What's worth keeping is the creator's call; delete doesn't even look in the bottle."""
        fake = self.fake()
        bottle = self.create(git_repo=self.repo_path)
        fake.calls.clear()
        bottles.delete(bottle.name)
        self.assertNotIn("init_workspace", fake.calls)  # no command was run in it
        self.assertEqual(fake.containers, {})
        self.assertEqual(run("git", "-C", self.repo_path, "remote"), "")


class StatusTest(BottleTestCase):
    def test_every_bottle_or_those_named(self) -> None:
        fake = self.fake()
        a = self.create()
        b = bottles.create({"features": []}, "spec", git_repo=self.repo_path,
                                     remotes=["work:fetch=ns/t-in,push=ns/t-out"])
        fake.containers[b.container] = "stopped"
        self.assertEqual([(r["name"], r["state"]) for r in bottles.status()], [("example", "running"), ("spec", "stopped")])
        [row, missing] = bottles.status(["spec", "gone"])
        self.assertEqual(missing, {"name": "gone", "state": "missing"})
        self.assertEqual(row | {"created": None}, {
            "name": "spec", "state": "stopped", "created": None, "git_repo": str(self.repo_path),
            "branch": "main", "commit": self.head, "features": [], "memory": "all",
            "spec": {"features": [], "memory": "all"}, "remotes": ["work:fetch=ns/t-in,push=ns/t-out"],
        })
        self.assertEqual(bottles.status([a.name])[0]["remotes"], ["origin:fetch=upstream", "host:fetch=heads"])
        self.assertEqual(json.loads(json.dumps(bottles.status())), bottles.status())


class SpecWorkspaceTest(BottleTestCase, unittest.IsolatedAsyncioTestCase):
    """A spec bottle's setup script, run for real against an egress proxy serving its remotes."""

    async def asyncSetUp(self) -> None:
        from bottle import egress

        host._last_pull.clear()
        self.fake_runtime = self.fake()
        run("git", "-C", self.repo_path, "branch", "feature")
        self.bottle = bottles.create(
            {"features": []}, "t", git_repo=self.repo_path, checkout="feature",
            remotes=["origin:fetch=upstream", "host:fetch=heads", "work:fetch=ns/t-in,push=ns/t-out"],
            context="Extra.\n",
        )
        services = host.HostServices(bottles.git_dir(self.bottle), self.tmp / "view.git", remotes=self.bottle.remotes)
        proxy = await egress.EgressProxy("t", egress.Policy(), services=services).start("127.0.0.1", 0)
        self.addAsyncCleanup(self._close, proxy)
        self.workspace, self.home = self.tmp / "workspace", self.tmp / "home"
        self.home.mkdir()
        self.env = {**__import__("os").environ, "HOME": str(self.home),
                    "http_proxy": f"http://127.0.0.1:{proxy.sockets[0].getsockname()[1]}"}
        script = (self.fake_runtime.workspace_script.replace(bottles.OBJECTS_MOUNT, str(self.repo_path / ".git/objects"))
                  .replace(bottles.WORKSPACE, str(self.workspace)))
        result = await self.run_(["sh", "-c", script])
        self.assertEqual(result.returncode, 0, result.stderr)

    @staticmethod
    async def _close(server) -> None:
        server.close()
        await server.wait_closed()

    async def run_(self, argv):
        import subprocess

        return await __import__("asyncio").to_thread(
            subprocess.run, argv, cwd=self.tmp, env=self.env, capture_output=True, text=True)

    async def git(self, *args):
        return await self.run_(["git", "-C", str(self.workspace), *args])

    async def test_checks_out_the_branch_borrowing_the_repos_objects(self) -> None:
        self.assertEqual(run("git", "-C", self.workspace, "symbolic-ref", "--short", "HEAD"), "feature")
        self.assertEqual(run("git", "-C", self.workspace, "rev-parse", "HEAD"), self.head)
        self.assertEqual(run("git", "-C", self.workspace, "count-objects"), "0 objects, 0 kilobytes")
        self.assertEqual(run("git", "-C", self.workspace, "status", "--porcelain"), "")

    async def test_fetched_its_remotes(self) -> None:
        self.assertIn("host/feature", run("git", "-C", self.workspace, "branch", "-r", "--format=%(refname:short)").split())
        self.assertEqual(run("git", "-C", self.workspace, "remote").split(), ["host", "origin", "work"])

    async def test_writes_the_context(self) -> None:
        # The script had /workspace rewritten to the stand-in, and so did the context in it.
        expected = bottles.context(self.bottle).replace(bottles.WORKSPACE, str(self.workspace))
        self.assertEqual((self.home / "BOTTLE.md").read_text(), expected)

    async def test_a_push_to_work_lands_in_its_namespace(self) -> None:
        commit = self.commit(self.workspace, "agent work")
        result = await self.git("push", "work", "feature")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(run("git", "-C", self.repo_path, "rev-parse", "refs/namespaces/t-out/refs/heads/feature"), commit)
        self.assertNotEqual(run("git", "-C", self.repo_path, "rev-parse", "feature"), commit)
