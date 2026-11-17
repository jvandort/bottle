//! Bottles: a sandbox, in its own VM, made from a spec and served the git remotes it's given.
//!
//! A bottle's record is written before anything is created, so a failure at any point leaves a
//! record of what may exist. It's kept with the thread it's for, in its directory in folio's home:
//! <repo key>/<thread>/bottles/<name>.json. Creation rolls back on failure; delete() removes
//! whatever parts exist, so it also cleans up a bottle that was left half-made.
//!
//! Each bottle has:
//!   - a machine, <name> (see bottle_cli.rs), with each of its mounts' repos' objects mounted
//!     read-only, and the network policy it decides its connections by (network), which routes
//!     some to folio serve: its services (git: each mount's git remotes, at git.bottle.internal;
//!     messages; the bottles running), ssh to bottles, and its credentials' hosts (see
//!     services.rs; serve/)
//!   - for each mount, a checkout at its path that borrows those objects via alternates. Which paths
//!     is the bottle's creator's to say; none is special, but the first is where a shell or a
//!     command starts (genie's home without one).
//!
//! What it installs, and its memory, come from a spec its creator owns (see specs.rs). Its remotes
//! are the ones it was created with, and nothing is registered in the repo: what the bottle pushes
//! is for its creator to read. What's worth keeping before a bottle is deleted is its creator's call
//! too.
//!
//! A bottle may ssh to other bottles its creator names (ssh: {NAME: BOTTLE}), and to nothing else
//! that way: NAME, in its /etc/hosts, is an address whose port 22 folio serve joins to sshd in that
//! bottle, the one that existed when the grant was made. Its key is let in there and that bottle's
//! host key is known here, when it's made: so that bottle must exist first, and be made again with
//! it, or the grant reaches nothing. The two may push into the same namespaces, which no other two
//! bottles may: they work together.
//!
//! Objects the bottle uses from the repo aren't protected from `git gc` in the hub: if the user
//! deletes a branch a bottle checked out and gc later prunes its commits (after git's grace
//! periods, weeks to months), that checkout breaks, and the bottle has to be made again.

mod bind;
mod context;
mod create;
mod machine;
mod state;

pub use bind::bindings;
pub use create::{MountRequest, Order, create};
pub use machine::{delete, ensure_running, exec_command, runner_of, start};
pub use state::{Known, State, Status, status};

mod record;

pub use record::{Bottle, Mount, SshGrant, get, load, touch};

pub const USER: &str = "genie";
/// In genie's home.
const CONTEXT_FILE: &str = "BOTTLE.md";
/// The remote a checkout's branches default to (git's checkout.defaultRemote), if it has one.
const ORIGIN: &str = "origin";
/// The version of the policy network() makes: 1, its routes to folio serve's one socket; 2, folio
/// serve's, by service; 3, on a runner, to folio serve's forwarded there (services_sockets), not
/// bottle-cli's; 4, a socket for each service, git, messages and the bottles routed by their names
/// under bottle.internal (bottle.host's paths before). A bottle given 4 has its names and remotes
/// pointed there again (machine.rs's ensure_running).
const NETWORK: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::specs::Spec;

    use super::record::Made;
    use crate::shell::quote as q;
    use crate::specs;

    use super::context::context;
    use super::context::machine_env;
    use super::machine::{CONTRACT, memory_mib};
    use crate::tempdir;
    use serde_json::{Value, json};

    #[test]
    fn the_contract_checks_run_and_name_what_failed() {
        // Run the real checks here: the script itself must be well-formed. Which fail depends on the
        // machine, so only require that whatever it reports is contract names and nothing else.
        let checks: Vec<String> = CONTRACT
            .iter()
            .map(|(what, test)| format!("sh -c {} >/dev/null 2>&1 || echo {}", q(test), q(what)))
            .collect();
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("# bottle contract\n{}", checks.join("\n"))])
            .output()
            .unwrap();
        assert!(out.status.success());
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            assert!(CONTRACT.iter().any(|(what, _)| *what == line), "{line}");
        }
    }

    #[test]
    fn a_bottle_gets_stand_ins_and_its_permissions_and_where_a_login_starts() {
        let _env = tempdir::isolated();
        let env = machine_env(&["claude".into()], &[], "/workspace").unwrap();
        assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], crate::auth::STANDIN);
        assert_eq!(
            env["BOTTLE_MIRROR_ENV"],
            "BOTTLE_WORKDIR CLAUDE_CODE_OAUTH_TOKEN"
        );
        let env = machine_env(&["tools".into()], &[], "/w").unwrap();
        assert_eq!(
            env,
            BTreeMap::from([
                ("BOTTLE_WORKDIR".into(), "/w".into()),
                ("BOTTLE_MIRROR_ENV".into(), "BOTTLE_WORKDIR".into())
            ])
        );
        let env = machine_env(
            &["tools".into()],
            &["list-bottles".into(), "message-any".into()],
            "/w",
        )
        .unwrap();
        assert_eq!(env["BOTTLE_PERMISSIONS"], "list-bottles,message-any");
        assert_eq!(
            env["BOTTLE_MIRROR_ENV"],
            "BOTTLE_PERMISSIONS BOTTLE_WORKDIR"
        );
    }

    #[test]
    fn its_context_says_what_its_features_and_permissions_give_it() {
        let _env = tempdir::isolated();
        let bottle = |features: &[&str], spec: Value| Bottle {
            name: "b".into(),
            id: "id".into(),
            image: "base".into(),
            created: 0.0,
            status: Made::Ready,
            features: features.iter().map(|f| f.to_string()).collect(),
            spec: Spec::of(
                json!({"memory": "2G", "permissions": spec["permissions"].clone()
                .as_array().cloned().unwrap_or_default()}),
            ),
            mounts: vec![],
            context: String::new(),
            runner: "local".into(),
            ssh: vec![],
            network_version: 0,
            last_active: 0.0,
        };
        let context_of = |b: &Bottle| context(b).unwrap();
        let text = context_of(&bottle(&["jvm:version=21", "tools"], json!({})));
        assert!(
            text.contains(
                "\n## Features\n\n### Feature 'jvm'\n\nTemurin JDK 21 is the default `java`"
            ),
            "{text}"
        );
        assert!(text.contains("\n### Feature 'tools'\n\n"));
        assert!(!context_of(&bottle(&[], json!({}))).contains("## Features"));
        let text = context_of(&bottle(&[], json!({"permissions": ["message-any"]})));
        assert!(
            text.contains("## Permissions")
                && text.contains(specs::PERMISSIONS[1].1)
                && !text.contains(specs::PERMISSIONS[0].1)
        );
        let refused = "comes back 403, saying why. That's policy, not a fault: if you need it, tell the user.";
        assert!(context_of(&bottle(&["github"], json!({}))).contains(refused));
        assert!(!context_of(&bottle(&["tools"], json!({}))).contains(refused));
        let detached = Bottle {
            mounts: vec![Mount::new(
                "/workspace",
                "/src/r",
                None,
                "c".repeat(40),
                vec![],
            )],
            ..bottle(&[], json!({}))
        };
        assert!(context_of(&detached).contains("checkout of `r`, at a detached commit."));
        assert_eq!(bottle(&[], json!({})).workdir(), "/home/genie");
    }

    #[test]
    fn memory_in_mib() {
        assert_eq!(memory_mib(Some("8G")).unwrap(), 8192);
        assert_eq!(memory_mib(Some("512m")).unwrap(), 512);
        assert_eq!(
            memory_mib(None).unwrap_err(),
            "a bottle needs its memory, a size in M or G, e.g. \"8G\" (not none)"
        );
    }
}
