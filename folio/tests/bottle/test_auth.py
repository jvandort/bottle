import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from folio.bottle import auth, features, krun
from folio.bottle.errors import BottleError

class FakeKeychain:
    """Stands in for `security`, recording every command line it's given."""

    def __init__(self) -> None:
        self.items: dict[tuple[str, str], str] = {}
        self.argvs: list[list[str]] = []

    def run(self, argv, input=None, capture_output=False, text=False):
        self.argvs.append(argv)
        if argv[:2] == ["security", "-i"]:
            words = input.split('"')
            service, account, value = words[1], words[3], words[5]
            self.items[(service, account)] = value
            return subprocess.CompletedProcess(argv, 0, "", "")
        service, account = argv[argv.index("-s") + 1], argv[argv.index("-a") + 1]
        if argv[1] == "find-generic-password":
            value = self.items.get((service, account))
            return subprocess.CompletedProcess(argv, 0 if value else 44, (value or "") + "\n", "")
        if argv[1] == "delete-generic-password":
            return subprocess.CompletedProcess(argv, 0 if self.items.pop((service, account), None) else 44, "", "")
        raise AssertionError(argv)


class StoreTest(unittest.TestCase):
    def setUp(self) -> None:
        self.keychain = FakeKeychain()
        for patcher in (
            mock.patch.object(auth.subprocess, "run", side_effect=self.keychain.run),
            mock.patch.dict(os.environ, {"BOTTLE_HOME": str(Path.home() / ".bottle")}),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_put_get_delete(self) -> None:
        self.assertIsNone(auth.get("claude"))
        auth.put("claude", "tok-123")
        self.assertEqual(auth.get("claude"), "tok-123")
        self.assertTrue(auth.delete("claude"))
        self.assertFalse(auth.delete("claude"))
        self.assertIsNone(auth.get("claude"))

    def test_values_never_go_on_a_command_line(self) -> None:
        auth.put("claude", "tok-secret")
        self.assertFalse(any("tok-secret" in arg for argv in self.keychain.argvs for arg in argv))

    def test_rejects_values_it_cant_store_safely(self) -> None:
        for value in ("", 'a"b', "a\\b", "a\nb", "a\tb"):
            with self.subTest(value), self.assertRaises(BottleError):
                auth.put("claude", value)

    def test_default_home_uses_the_plain_service(self) -> None:
        self.assertEqual(auth._service(), "bottle")

    def test_other_homes_get_their_own_service(self) -> None:
        with mock.patch.dict(os.environ, {"BOTTLE_HOME": "/tmp/elsewhere"}):
            self.assertRegex(auth._service(), r"^bottle:[0-9a-f]{12}$")


class DeclaredTest(unittest.TestCase):
    def test_claude_declares_its_token(self) -> None:
        d = auth.get_declared("claude")
        self.assertEqual((d.feature, d.credential.inject.hosts), ("claude", ("api.anthropic.com",)))

    def test_unknown(self) -> None:
        with self.assertRaisesRegex(BottleError, "no feature declares a credential named 'nope' \\(known: claude, develocity, github, teamcity\\)"):
            auth.get_declared("nope")

    def test_only_the_bottles_features_are_injected(self) -> None:
        with mock.patch.object(auth, "get", return_value="tok"):
            [claude] = auth.injections_for(["claude", "tools"])
            self.assertEqual((claude.host, claude.value), ("api.anthropic.com", "Bearer tok"))
            self.assertEqual(auth.injections_for(["tools"]), [])


class StandinTest(unittest.TestCase):
    """What a bottle gets instead of a credential: a fake token, so a tool starts at all."""

    def test_a_feature_names_the_variable_its_tool_reads(self) -> None:
        self.assertEqual(auth.standins_for(["claude", "tools"]), {"CLAUDE_CODE_OAUTH_TOKEN": features.STANDIN})

    def test_a_feature_that_configures_its_own_asks_for_none(self) -> None:
        # teamcity writes its stand-in into the CLI's config file, in install.sh.
        self.assertEqual(auth.standins_for(["teamcity:server=ci.example.com"]), {})

    def test_a_stand_in_a_feature_writes_itself_is_the_one_the_proxy_replaces(self) -> None:
        # Anything else would reach the server as it is, and the CLI would be refused.
        install = Path(__file__).resolve().parents[2] / "features/teamcity/install.sh"
        self.assertIn(f"token: {features.STANDIN}", install.read_text())

    def test_the_keychain_is_never_read_for_one(self) -> None:
        with mock.patch.object(auth, "get", side_effect=AssertionError("a stand-in is not a credential")):
            self.assertEqual(auth.standins_for(["claude"]), {"CLAUDE_CODE_OAUTH_TOKEN": features.STANDIN})


class InjectedCredentialTest(unittest.TestCase):
    """A credential bottled holds: nothing about it enters the bottle."""

    specs = ["teamcity:server=https://ci.corp.example.com", "tools"]

    def test_the_proxy_gets_the_real_one(self) -> None:
        with mock.patch.object(auth, "get", return_value="tc-token"):
            [injection] = auth.injections_for(self.specs)
        self.assertEqual(injection.host, "ci.corp.example.com")
        self.assertEqual((injection.header, injection.value), ("Authorization", "Bearer tc-token"))
        self.assertEqual(injection.port, 443)
        # Given in place of the stand-in only, and bare for Basic credentials.
        self.assertEqual((injection.standin, injection.credential), (features.STANDIN, "tc-token"))

    def test_nothing_is_injected_until_it_is_logged_in(self) -> None:
        with mock.patch.object(auth, "get", return_value=None):
            self.assertEqual(auth.injections_for(self.specs), [])

    def test_its_machine_routes_its_credentials_hosts_and_ports_logged_in_or_not(self) -> None:
        with mock.patch.object(auth, "get", return_value=None):
            self.assertEqual(auth.credential_destinations(self.specs), [("ci.corp.example.com", 443)])

    def test_one_host_belongs_to_one_credential(self) -> None:
        claimed = features.Credential(
            "other", "Another token",
            inject=features.Inject(hosts=("ci.corp.example.com",), value="Bearer ${credential}"),
        )
        with mock.patch.object(auth, "get", return_value="tok"), \
                mock.patch.object(auth, "credentials_for", return_value=[
                    auth.credentials_for(self.specs)[0], claimed]), \
                self.assertRaisesRegex(BottleError, "both claim ci.corp.example.com"):
            auth.injections_for(self.specs)


class PropertiesTest(unittest.TestCase):
    """Credentials read from a file a tool on the machine keeps them in, as Gradle does for Develocity."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.keys = Path(tmp.name) / "keys.properties"
        self.keys.write_text(
            "# written by provisionDevelocityAccessKey\n"
            "ge.example.com=ge-key\n"
            "https://dv.corp.example.com/ = dv-key\n"
            "other.example.com\\:8443:port-key\n"
            "\n! another comment\nempty.example.com=\n"
        )
        self.specs = [f"develocity:server=https://dv.corp.example.com,keyFile={self.keys}"]

    def test_reads_lines_by_key(self) -> None:
        self.assertEqual(auth.read_properties(self.keys), {
            "ge.example.com": "ge-key", "https://dv.corp.example.com/": "dv-key",
            "other.example.com:8443": "port-key", "empty.example.com": "",
        })

    def test_no_file_is_no_lines(self) -> None:
        self.assertEqual(auth.read_properties(self.keys.with_name("nope")), {})

    def test_the_servers_line_is_attached_without_the_keychain(self) -> None:
        with mock.patch.object(auth, "get", side_effect=AssertionError("the file has it")):
            [injection] = auth.injections_for(self.specs)
        self.assertEqual((injection.host, injection.value), ("dv.corp.example.com", "Bearer dv-key"))

    def test_each_server_gets_its_own_line(self) -> None:
        specs = [f"develocity:server=https://ge.example.com,https://dv.corp.example.com,keyFile={self.keys}"]
        with mock.patch.object(auth, "get", return_value=None):
            injections = auth.injections_for(specs)
        self.assertEqual([(i.host, i.value) for i in injections],
                         [("ge.example.com", "Bearer ge-key"), ("dv.corp.example.com", "Bearer dv-key")])
        self.assertEqual(auth.credential_destinations(specs), [("dv.corp.example.com", 443), ("ge.example.com", 443)])

    def test_a_host_with_no_line_falls_back_to_the_keychain(self) -> None:
        specs = [f"develocity:server=https://new.example.com,keyFile={self.keys}"]
        with mock.patch.object(auth, "get", return_value="stored-key"):
            [injection] = auth.injections_for(specs)
        self.assertEqual(injection.value, "Bearer stored-key")
        with mock.patch.object(auth, "get", return_value=None):
            self.assertEqual(auth.injections_for(specs), [])

    def test_an_empty_line_is_no_credential(self) -> None:
        [credential] = auth.credentials_for([f"develocity:server=empty.example.com,keyFile={self.keys}"])
        self.assertIsNone(auth.from_properties(credential, "empty.example.com"))

    def test_a_host_with_a_port_matches_its_own_line(self) -> None:
        [credential] = auth.credentials_for([f"develocity:server=other.example.com:8443,keyFile={self.keys}"])
        self.assertEqual(auth.from_properties(credential, "other.example.com:8443"), "port-key")

    def test_the_default_file_is_gradles(self) -> None:
        [credential] = auth.credentials_for(["develocity:server=dv.corp.example.com"])
        self.assertEqual(credential.properties, "~/.gradle/develocity/keys.properties")

    def test_nothing_unset_when_the_file_has_it(self) -> None:
        with mock.patch.object(auth, "get", return_value=None), mock.patch("sys.stderr") as err:
            auth.warn_unset(self.specs)
        err.write.assert_not_called()


class DestinationTest(unittest.TestCase):
    def test_a_host_is_reached_over_https(self) -> None:
        self.assertEqual(auth.destination("ci.example.com"), ("ci.example.com", 443))
        self.assertEqual(auth.destination("*.example.com"), ("*.example.com", 443))

    def test_a_host_may_name_its_port(self) -> None:
        self.assertEqual(auth.destination("ci.example.com:8111"), ("ci.example.com", 8111))

    def test_a_host_is_only_a_host(self) -> None:
        for host in ("ci.example.com:https", "ci.example.com:0", "ci.example.com:99999"):
            with self.subTest(host), self.assertRaisesRegex(BottleError, "isn't a host for a credential"):
                auth.destination(host)


class WarnUnsetTest(unittest.TestCase):
    def said(self, specs: list[str]) -> str:
        with mock.patch("sys.stderr") as err:
            auth.warn_unset(specs)
        return "".join(c.args[0] for c in err.write.call_args_list)

    def test_says_how_to_set_what_isnt(self) -> None:
        with mock.patch.object(auth, "get", return_value=None):
            self.assertIn("claude", self.said(["claude", "tools"]))
            self.assertIn("`folio auth set claude`", self.said(["claude"]))

    def test_nothing_when_set(self) -> None:
        with mock.patch.object(auth, "get", return_value="tok"):
            self.assertEqual(self.said(["claude"]), "")

    def test_features_without_credentials(self) -> None:
        self.assertEqual(self.said(["tools"]), "")


class ExecEnvTest(unittest.TestCase):
    def test_nothing_from_the_host_reaches_a_session(self) -> None:
        """A bottle's environment is its image's: no credential is ever handed in here."""
        with mock.patch.dict(os.environ, {"TOKEN": "tok-secret"}), mock.patch.object(krun.prereqs, "ensure_cli"):
            argv = krun.exec_command("m", ["bash"], user="genie", tty=True)
        self.assertNotIn("-e", argv)
        self.assertFalse(any("tok-secret" in arg for arg in argv))


if __name__ == "__main__":
    unittest.main()
