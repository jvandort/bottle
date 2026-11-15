//! Credentials: logged in to once on the controller, and used by the bottles that need them.
//!
//! Features declare the credentials they need (customizations.bottle.credentials in their
//! devcontainer-feature.json): a name, and where the credential goes. Each is stored in the macOS
//! Keychain, set from stdin (`folio auth set`).
//!
//! A credential never enters a bottle. folio serve reads it when a bottle is bound
//! (serve/credentials.rs) and puts it in place of the stand-in on the bottle's requests to the hosts
//! the feature named, on HTTPS connections it makes itself. The agent can spend the credential
//! against those hosts, and can't read it -- so a bottle that leaks everything it holds leaks no
//! credential.
//!
//! What a bottle gets instead is a stand-in (standins_for): a fake token in the variable a feature
//! named, because a CLI doesn't know its token is being replaced for it and won't work until it
//! thinks it's logged in. A request that presents the stand-in is one that asks for the credential;
//! the rest reach the host as they would from the machine.
//!
//! A credential can also come from a file a tool on the machine already keeps its credentials in
//! (the credential's properties, see features.rs), such as Gradle's
//! ~/.gradle/develocity/keys.properties: folio serve reads the line for each host, and the Keychain
//! is for the hosts the file doesn't name.
//!
//! A bottle picks up a credential set or removed, or a change to such a file, when it next starts.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::features::{self, Credential};
use crate::serve::credentials::{STANDIN, destination, keychain, read_properties, service};

/// A credential, and a feature that declares it.
pub struct Declared {
    pub credential: Credential,
    pub feature: String,
}

/// Every credential the available features declare, by name.
pub fn declared() -> Result<BTreeMap<String, Declared>, String> {
    let mut result: BTreeMap<String, Declared> = BTreeMap::new();
    for id in features::available() {
        for credential in features::load(&id)?.credentials {
            if let Some(other) = result.get(&credential.name) {
                if other.credential != credential {
                    return Err(format!(
                        "features {} and {id} declare credential {} differently",
                        other.feature,
                        crate::repr(&credential.name)
                    ));
                }
                continue;
            }
            result.insert(
                credential.name.clone(),
                Declared {
                    credential,
                    feature: id.clone(),
                },
            );
        }
    }
    Ok(result)
}

fn get_declared(name: &str) -> Result<Declared, String> {
    let mut known = declared()?;
    known.remove(name).ok_or_else(|| {
        let names: Vec<&str> = known.keys().map(String::as_str).collect();
        format!(
            "no feature declares a credential named {} (known: {})",
            crate::repr(name),
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        )
    })
}

// --- storage: the macOS Keychain -------------------------------------------------

pub fn get(name: &str) -> Option<String> {
    keychain(name)
}

/// Store `value`. It goes to `security` on stdin, never on a command line.
fn put(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.contains(['"', '\\', '\n', '\r'])
        || !value.chars().all(|c| !c.is_control())
    {
        return Err(format!(
            "{name}: the value is empty or has characters bottle can't store (quotes, backslashes, control)"
        ));
    }
    let command = format!(
        "add-generic-password -U -s \"{}\" -a \"{name}\" -w \"{value}\"\n",
        service()
    );
    let failed = |why: String| format!("couldn't store {name} in the Keychain: {why}");
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(e.to_string()))?;
    let _ = child.stdin.take().unwrap().write_all(command.as_bytes());
    let output = child
        .wait_with_output()
        .map_err(|e| failed(e.to_string()))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success()
        || stderr.to_lowercase().contains("error")
        || get(name).as_deref() != Some(value)
    {
        return Err(failed(if stderr.is_empty() {
            "unknown error".into()
        } else {
            stderr
        }));
    }
    crate::log::log!("auth", "credential {name} stored");
    Ok(())
}

/// Remove the credential. False if it wasn't stored.
fn delete(name: &str) -> bool {
    Command::new("security")
        .args(["delete-generic-password", "-s", &service(), "-a", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// --- commands -------------------------------------------------------------------

/// Every declared credential, and whether the Keychain has it.
pub fn status() -> Result<Vec<(Declared, bool)>, String> {
    Ok(declared()?
        .into_iter()
        .map(|(name, d)| {
            let set = get(&name).is_some();
            (d, set)
        })
        .collect())
}

pub fn set_value(name: &str, value: &str) -> Result<(), String> {
    get_declared(name)?;
    put(name, value.trim())
}

pub fn remove(name: &str) -> Result<bool, String> {
    get_declared(name)?;
    let removed = delete(name);
    if removed {
        crate::log::log!("auth", "credential {name} removed");
    }
    Ok(removed)
}

// --- injection ------------------------------------------------------------------

/// The credential for `host`, one of its inject.hosts, from its properties file; None when the file
/// has no line for it. A line's key may be the host or a URL of it; a host with a port matches its
/// host:port line, else its bare host's.
fn from_properties(credential: &Credential, host: &str) -> Result<Option<String>, String> {
    let Some(properties) = &credential.properties else {
        return Ok(None);
    };
    let mut keys = BTreeMap::new();
    for (key, value) in read_properties(properties)? {
        if !value.is_empty() {
            let key = key.to_lowercase();
            let key = key
                .strip_prefix("https://")
                .unwrap_or(&key)
                .trim_end_matches('/')
                .to_string();
            keys.insert(key, value);
        }
    }
    let (pattern, _) = destination(host)?;
    Ok(keys
        .get(&host.to_lowercase())
        .or_else(|| keys.get(&pattern.to_lowercase()))
        .cloned())
}

/// The credentials a bottle with these features gets, with their features' options filled in.
fn credentials_for(specs: &[String]) -> Result<Vec<Credential>, String> {
    let mut result = Vec::new();
    for feature in features::resolve(specs)? {
        result.extend(feature.resolved_credentials()?);
    }
    Ok(result)
}

/// Say which credentials these features need that aren't set, and how to set them; the bottle goes
/// on without, reaching their hosts unauthenticated.
pub fn warn_unset(specs: &[String]) -> Result<(), String> {
    for credential in credentials_for(specs)? {
        let mut all = true;
        for host in &credential.inject.hosts {
            if from_properties(&credential, host)?.is_none() {
                all = false;
                break;
            }
        }
        if all || get(&credential.name).is_some() {
            continue;
        }
        crate::log::say!(
            "auth",
            "bottle: {} ({}) isn't set: `folio auth set {}`",
            credential.name,
            credential.description,
            credential.name
        );
    }
    Ok(())
}

/// The host patterns and ports a bottle with these features has credentials attached at, logged in
/// or not: what its machine routes to folio serve (see services::network).
pub fn credential_destinations(specs: &[String]) -> Result<Vec<(String, u16)>, String> {
    let mut result = Vec::new();
    for credential in credentials_for(specs)? {
        for host in &credential.inject.hosts {
            let (pattern, port) = destination(host)?;
            result.push((pattern.to_lowercase(), port));
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}

/// The fake tokens a bottle with these features gets, by variable name. A stand-in is not a
/// credential and not a secret: it is what makes a tool believe it's logged in, so it will make the
/// request folio serve then authenticates. bottle sets these when it creates a bottle.
pub fn standins_for(specs: &[String]) -> Result<BTreeMap<String, String>, String> {
    Ok(credentials_for(specs)?
        .into_iter()
        .filter_map(|c| c.inject.standin.map(|s| (s, STANDIN.to_string())))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fake_security");

    /// The Keychain, as tests/fake_security keeps it, first on PATH as `security`.
    struct Keychain {
        _env: tempdir::Isolated,
        tmp: tempdir::Dir,
    }

    impl Keychain {
        fn new() -> Keychain {
            let env = tempdir::isolated();
            let tmp = tempdir::Dir::new();
            let bin = tmp.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            std::os::unix::fs::symlink(FAKE, bin.join("security")).unwrap();
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            tempdir::set(&env, "PATH", &path);
            tempdir::set(
                &env,
                "FAKE_KEYCHAIN",
                tmp.path().join("keychain").to_str().unwrap(),
            );
            let home = std::env::var("HOME").unwrap_or_default();
            tempdir::set(&env, "BOTTLE_HOME", &format!("{home}/.bottle"));
            Keychain { _env: env, tmp }
        }

        fn argv(&self) -> String {
            std::fs::read_to_string(self.tmp.path().join("keychain/argv")).unwrap_or_default()
        }
    }

    #[test]
    fn put_get_delete() {
        let _keychain = Keychain::new();
        assert_eq!(get("claude"), None);
        put("claude", "tok-123").unwrap();
        assert_eq!(get("claude").as_deref(), Some("tok-123"));
        assert!(delete("claude"));
        assert!(!delete("claude"));
        assert_eq!(get("claude"), None);
    }

    #[test]
    fn values_never_go_on_a_command_line() {
        let keychain = Keychain::new();
        put("claude", "tok-secret").unwrap();
        assert!(!keychain.argv().contains("tok-secret"));
    }

    #[test]
    fn rejects_values_it_cant_store_safely() {
        let keychain = Keychain::new();
        for value in ["", "a\"b", "a\\b", "a\nb", "a\tb"] {
            assert!(put("claude", value).is_err(), "{value:?}");
        }
        assert_eq!(keychain.argv(), "");
    }

    #[test]
    fn the_default_home_has_the_plain_service_and_others_their_own() {
        let keychain = Keychain::new();
        assert_eq!(service(), "bottle");
        tempdir::set(&keychain._env, "BOTTLE_HOME", "/tmp/elsewhere");
        let other = service();
        assert!(
            other.starts_with("bottle:") && other.len() == "bottle:".len() + 12,
            "{other}"
        );
    }

    #[test]
    fn set_and_remove_are_for_declared_credentials_only() {
        let _keychain = Keychain::new();
        set_value("claude", "  tok \n").unwrap();
        assert_eq!(get("claude").as_deref(), Some("tok"));
        assert!(remove("claude").unwrap());
        assert!(!remove("claude").unwrap());
        assert_eq!(
            set_value("nope", "x").unwrap_err(),
            "no feature declares a credential named 'nope' (known: claude, develocity, github, teamcity)"
        );
        let status: Vec<(String, String, bool)> = status()
            .unwrap()
            .into_iter()
            .map(|(d, set)| (d.credential.name, d.feature, set))
            .collect();
        assert_eq!(
            status[0],
            ("claude".to_string(), "claude".to_string(), false)
        );
    }

    #[test]
    fn a_feature_names_the_variable_its_tool_reads_for_its_stand_in() {
        let _env = tempdir::isolated();
        let specs = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            standins_for(&specs(&["claude", "tools"])).unwrap(),
            BTreeMap::from([("CLAUDE_CODE_OAUTH_TOKEN".to_string(), STANDIN.to_string())])
        );
        // teamcity writes its stand-in into the CLI's config file, in install.sh: the one the
        // proxy replaces.
        assert!(
            standins_for(&specs(&["teamcity:server=ci.example.com"]))
                .unwrap()
                .is_empty()
        );
        let install =
            std::fs::read_to_string(crate::features::dir().join("teamcity/install.sh")).unwrap();
        assert!(install.contains(&format!("token: {STANDIN}")));
        assert_eq!(
            credential_destinations(&specs(&[
                "teamcity:server=https://ci.corp.example.com",
                "tools"
            ]))
            .unwrap(),
            [("ci.corp.example.com".to_string(), 443)]
        );
    }

    #[test]
    fn credentials_from_a_file_a_tool_keeps_them_in() {
        let _env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let keys = tmp.path().join("keys.properties");
        std::fs::write(
            &keys,
            "# written by provisionDevelocityAccessKey\nge.example.com=ge-key\nhttps://dv.corp.example.com/ = dv-key\n\
             other.example.com\\:8443:port-key\n\n! another comment\nempty.example.com=\n",
        )
        .unwrap();
        let keys = keys.display();
        let one = |spec: String| credentials_for(&[spec]).unwrap().remove(0);
        let both = format!(
            "develocity:server=https://ge.example.com,https://dv.corp.example.com,keyFile={keys}"
        );
        assert_eq!(
            credential_destinations(&[both]).unwrap(),
            [
                ("dv.corp.example.com".to_string(), 443),
                ("ge.example.com".to_string(), 443)
            ]
        );
        let credential = one(format!(
            "develocity:server=empty.example.com,keyFile={keys}"
        ));
        assert_eq!(
            from_properties(&credential, "empty.example.com").unwrap(),
            None
        );
        let credential = one(format!(
            "develocity:server=other.example.com:8443,keyFile={keys}"
        ));
        assert_eq!(
            from_properties(&credential, "other.example.com:8443")
                .unwrap()
                .as_deref(),
            Some("port-key")
        );
        let credential = one(format!(
            "develocity:server=https://dv.corp.example.com,keyFile={keys}"
        ));
        assert_eq!(
            from_properties(&credential, "dv.corp.example.com")
                .unwrap()
                .as_deref(),
            Some("dv-key")
        );
        let credential = one("develocity:server=dv.corp.example.com".into());
        assert_eq!(
            credential.properties.as_deref(),
            Some("~/.gradle/develocity/keys.properties")
        );
    }

    #[test]
    fn a_credential_goes_over_https_to_a_host_or_a_host_and_port() {
        assert_eq!(
            destination("ci.example.com").unwrap(),
            ("ci.example.com".to_string(), 443)
        );
        assert_eq!(
            destination("*.example.com").unwrap(),
            ("*.example.com".to_string(), 443)
        );
        assert_eq!(
            destination("ci.example.com:8111").unwrap(),
            ("ci.example.com".to_string(), 8111)
        );
        for host in [
            "ci.example.com:https",
            "ci.example.com:0",
            "ci.example.com:99999",
        ] {
            assert!(
                destination(host)
                    .unwrap_err()
                    .contains("isn't a host for a credential"),
                "{host}"
            );
        }
    }
}
