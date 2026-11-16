//! Bottle images, built: Debian, then the base feature (the bottle contract), then a bottle's
//! features (images.rs works out what to build; this builds it, with bottle-cli, where a bottle is
//! made).
//!
//! An image is stages: ext4 disks, each what one build wrote on top of the stages below it, stacked
//! by overlayfs when a machine boots (see bottle-cli's build). Machines on the same stages share
//! them, and the host's cache of them: every bottle's image is Debian's stage, base's on it, and its
//! features' on that.
//!
//! A build is a recipe on a registry's image (pull: its reference) or on one of bottle's built here
//! (from: its tag): steps run in a VM, a directory of this repo mounted read-only where they run,
//! nothing of it copied in.
//!
//! Built images are bottle-cli's, in its store where it runs (here, or a runner), by their tags
//! (`bottle-cli images`); labels are the stage's. A stage's name is a hash of what it was built
//! from, so rebuilding a tag never replaces a stage a machine was made from; `bottle-cli gc` deletes
//! those nothing uses, keeping every tagged image and every machine's stages whatever its state.
//! folio names tags, and nothing of bottle-cli's store itself.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::bottle_cli::{Cli, images_cli};
use crate::features::BASE;
use crate::features::{self, Feature};
use crate::images::{self, REPOSITORY};
use crate::runners::Runner;
use crate::services;

/// Every image bottle builds records a hash of what it was built from. An image whose inputs have
/// changed since is stale: it's rebuilt when next needed, and deleted once no machine uses it.
const INPUTS_LABEL: &str = "bottle.inputs";
const FEATURES_LABEL: &str = "bottle.features";
/// Where a build's directory is in its VM: on /run's tmpfs, so nothing of it is on the stage.
const MOUNT: &str = "/run/bottle-build";

/// What the base image with `features` is built from: base's alone, without any.
fn inputs_of(features: &[Feature]) -> Result<String, String> {
    if features.is_empty() {
        images::base_inputs()
    } else {
        images::inputs_hash(features)
    }
}

/// Build the base image with the features in `specs` on `runner`, and the base image under it, if
/// missing or stale there; return its tag.
pub fn ensure_built(specs: &[String], runner: &Runner) -> Result<String, String> {
    let features = features::resolve(specs)?;
    let tag = images::image_tag(&features);
    let mut built = ensure_base(runner)?;
    if !features.is_empty() {
        let inputs = inputs_of(&features)?;
        if !is_current(&tag, &inputs, &runner.cli)? {
            crate::log::say!("images", "Building {tag}{}...", place(runner));
            let recipe = images::image(&features)?["recipe"].clone();
            build_recipe(&features::dir(), &tag, &inputs, &recipe, &runner.cli)?;
            built = true;
        }
    }
    if built {
        for name in remove_stale(runner)? {
            crate::log::say!("images", "Removed stale image {name}");
        }
    }
    Ok(tag)
}

fn place(runner: &Runner) -> String {
    if runner.name == crate::runners::LOCAL {
        String::new()
    } else {
        format!(" on runner {}", runner.name)
    }
}

/// The base image built on `runner` if it's missing or stale there; whether it was.
fn ensure_base(runner: &Runner) -> Result<bool, String> {
    let tag = images::tag(BASE);
    let inputs = images::base_inputs()?;
    if is_current(&tag, &inputs, &runner.cli)? {
        return Ok(false);
    }
    crate::log::say!("images", "Building {tag}{}...", place(runner));
    let recipe = images::base()?["recipe"].clone();
    build_recipe(&features::dir(), &tag, &inputs, &recipe, &runner.cli)?;
    Ok(true)
}

/// Forget bottle's images on `runner` whose inputs have changed since they were built; delete
/// stages nothing there uses.
pub fn remove_stale(runner: &Runner) -> Result<Vec<String>, String> {
    let mut removed = Vec::new();
    for (tag, labels) in built(&runner.cli)? {
        if labels.get(INPUTS_LABEL) != expected_inputs(&tag, &labels).as_ref() {
            forget(&tag, &runner.cli)?;
            removed.push(tag);
        }
    }
    images_cli(&runner.cli, &["gc"])?;
    Ok(removed)
}

/// The inputs hash an image with this tag and labels should have now; None if it can't be built
/// any more.
fn expected_inputs(tag: &str, labels: &BTreeMap<String, String>) -> Option<String> {
    if !tag.starts_with(&format!("{REPOSITORY}/{BASE}:")) {
        return None;
    }
    let Some(specs) = labels.get(FEATURES_LABEL) else {
        return if tag == images::tag(BASE) {
            images::base_inputs().ok()
        } else {
            None
        };
    };
    let specs: Vec<String> = specs.split_whitespace().map(String::from).collect();
    features::resolve(&specs).and_then(|f| inputs_of(&f)).ok()
}

/// Whether `image` is built where `cli` runs bottle-cli from what it's built from now.
fn is_current(image: &str, expected: &str, cli: &Cli) -> Result<bool, String> {
    Ok(built_labels(image, cli)?
        .is_some_and(|labels| labels.get(INPUTS_LABEL).map(String::as_str) == Some(expected)))
}

/// Build `recipe` into `image`, `context` mounted at its working directory (MOUNT): a stage, in a VM
/// of bottle-cli's; return its disk.
///
/// The build reaches the public network, with none of the features' credentials, every feature's
/// apt repositories kept (images::build_cached), and a step asking for BOTTLE_BUILD_CA gets, from
/// bottle-cli, the CA they're answered behind. `inputs` is the hash of what the image is built from,
/// recorded as a label. Built where `cli` runs bottle-cli: here, or on a runner, the context sent
/// there with the recipe.
fn build_recipe(
    context: &Path,
    image: &str,
    inputs: &str,
    recipe: &Value,
    cli: &Cli,
) -> Result<PathBuf, String> {
    if let Some(pull) = recipe.get("pull").and_then(Value::as_str) {
        crate::log::say!("images", "Pulling {pull}...");
    } else if built_labels(recipe["from"].as_str().unwrap_or_default(), cli)?.is_none() {
        return Err(format!(
            "{} isn't built",
            recipe["from"].as_str().unwrap_or_default()
        ));
    }
    let mut labels = recipe.get("labels").cloned().unwrap_or_else(|| json!({}));
    labels[INPUTS_LABEL] = inputs.into();
    let mut made = serde_json::Map::new();
    for key in ["pull", "from", "entrypoint", "cmd"] {
        if let Some(value) = recipe.get(key) {
            made.insert(key.into(), value.clone());
        }
    }
    made.insert("mount".into(), json!({"target": MOUNT}));
    made.insert(
        "steps".into(),
        recipe.get("steps").cloned().unwrap_or_else(|| json!([])),
    );
    made.insert("labels".into(), labels);
    // The public network, with none of its features' credentials, nothing routed to folio serve;
    // apt repositories kept.
    let network = json!({"cache": services::cache_rules(&images::build_cached()?)});
    // Its progress on stderr; the disk of the stage it made, the last line of its stdout.
    let result = cli.build(&Value::Object(made), &network, context, image)?;
    let stdout = String::from_utf8_lossy(&result.stdout);
    match stdout.split_whitespace().last() {
        Some(disk) if result.status.success() => Ok(PathBuf::from(disk)),
        _ => Err("bottle-cli image build failed".into()),
    }
}

/// bottle-cli's images: each tag, its stage, disks and labels.
fn images_of(cli: &Cli) -> Result<Vec<Value>, String> {
    let listed = images_cli(cli, &["images", "--json"])?;
    let images: Value =
        serde_json::from_str(&listed).map_err(|e| format!("bottle-cli images: {e}"))?;
    Ok(images.as_array().cloned().unwrap_or_default())
}

fn labels_of(image: &Value) -> BTreeMap<String, String> {
    image["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
        .collect()
}

/// The labels `image` was last built with, or None if it's not built.
fn built_labels(image: &str, cli: &Cli) -> Result<Option<BTreeMap<String, String>>, String> {
    Ok(images_of(cli)?
        .iter()
        .find(|i| i["tag"] == image && i["disks"].as_array().is_some_and(|d| !d.is_empty()))
        .map(labels_of))
}

/// Every built tag, with its labels.
fn built(cli: &Cli) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    Ok(images_of(cli)?
        .iter()
        .map(|i| {
            (
                i["tag"].as_str().unwrap_or_default().to_string(),
                labels_of(i),
            )
        })
        .collect())
}

/// Untag `image`; its stages go once nothing uses them (gc).
fn forget(image: &str, cli: &Cli) -> Result<(), String> {
    if images_of(cli)?.iter().any(|i| i["tag"] == image) {
        images_cli(cli, &["image", "rm", image])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir;
    use std::fs;

    /// A build test: features of its own (base, tools, and jvm on tools), and the stand-in
    /// bottle-cli.
    struct Builds {
        env: tempdir::Isolated,
        tmp: tempdir::Dir,
    }

    impl Drop for Builds {
        fn drop(&mut self) {
            features::TEST_DIR.with(|d| *d.borrow_mut() = None);
        }
    }

    impl Builds {
        fn new() -> Builds {
            let env = tempdir::isolated();
            let tmp = tempdir::Dir::new();
            tempdir::set(&env, "BOTTLE_CLI", &tempdir::standin_cli());
            tempdir::set(
                &env,
                "FAKE_CLI_STATE",
                tmp.path().join("state.json").to_str().unwrap(),
            );
            let dir = tmp.path().join("features");
            features::TEST_DIR.with(|d| *d.borrow_mut() = Some(dir));
            let builds = Builds { env, tmp };
            builds.feature("base", json!({"containerEnv": {"LANG": "C.UTF-8"}}));
            builds.feature("tools", json!({}));
            builds.feature("jvm", json!({"dependsOn": {"tools": {}}}));
            builds
        }

        fn feature(&self, id: &str, spec: Value) {
            let dir = self.tmp.path().join("features").join(id);
            fs::create_dir_all(&dir).unwrap();
            let mut spec = spec;
            spec["id"] = id.into();
            spec["version"] = "1.0.0".into();
            fs::write(dir.join("devcontainer-feature.json"), spec.to_string()).unwrap();
            fs::write(dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        }

        fn state(&self) -> Value {
            match fs::read_to_string(self.tmp.path().join("state.json")) {
                Ok(text) => serde_json::from_str(&text).unwrap(),
                Err(_) => json!({"images": [], "builds": [], "calls": []}),
            }
        }

        fn built(&self) -> Vec<String> {
            self.state()["builds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["tag"].as_str().unwrap().to_string())
                .collect()
        }

        fn image(&self, tag: &str, labels: Value) {
            let mut state = self.state();
            state["images"]
                .as_array_mut()
                .unwrap()
                .push(json!({"tag": tag, "stage": "s", "disks": ["s.ext4"], "labels": labels}));
            fs::write(self.tmp.path().join("state.json"), state.to_string()).unwrap();
        }

        fn local(&self) -> Runner {
            crate::runners::get(crate::runners::LOCAL).unwrap()
        }

        fn specs(specs: &[&str]) -> Vec<String> {
            specs.iter().map(|s| s.to_string()).collect()
        }
    }

    #[test]
    fn base_then_the_combination_is_built_then_whats_current_kept() {
        let b = Builds::new();
        assert_eq!(
            ensure_built(&Builds::specs(&["jvm"]), &b.local()).unwrap(),
            "localhost/bottle/base:with-jvm.tools"
        );
        assert_eq!(
            b.built(),
            [
                "localhost/bottle/base:latest",
                "localhost/bottle/base:with-jvm.tools"
            ]
        );
        assert!(
            b.state()["calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c[0] == "gc")
        ); // cleaned up after
        ensure_built(&Builds::specs(&["jvm"]), &b.local()).unwrap();
        assert_eq!(b.built().len(), 2);
        assert_eq!(
            ensure_built(&[], &b.local()).unwrap(),
            "localhost/bottle/base:latest"
        );
        assert_eq!(b.built().len(), 2);
    }

    #[test]
    fn base_is_the_base_feature_on_debian_running_bottles_entrypoint() {
        let b = Builds::new();
        ensure_built(&[], &b.local()).unwrap();
        let state = b.state();
        let [build] = &state["builds"].as_array().unwrap()[..] else {
            panic!()
        };
        let recipe = &build["recipe"];
        assert_eq!(
            (&recipe["pull"], &recipe["entrypoint"], &recipe["cmd"]),
            (
                &json!(images::DEBIAN),
                &json!(images::ENTRYPOINT),
                &json!(images::CMD)
            )
        );
        assert_eq!(recipe["steps"][0]["cwd"], format!("{MOUNT}/base"));
        assert_eq!(recipe["steps"][1], json!({"env": {"LANG": "C.UTF-8"}}));
        assert_eq!(
            recipe["mount"],
            json!({"source": b.tmp.path().join("features").to_str().unwrap(), "target": MOUNT})
        );
        assert_eq!(
            recipe["labels"],
            json!({INPUTS_LABEL: images::base_inputs().unwrap()})
        );
        // The public network, its apt repositories kept, and nothing routed.
        let network = build["network"].as_object().unwrap();
        assert_eq!(network.keys().collect::<Vec<_>>(), ["cache"]);
        assert!(
            network["cache"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["host"] == "deb.debian.org")
        );
    }

    #[test]
    fn a_combination_is_on_base_by_its_tag_and_says_its_features() {
        let b = Builds::new();
        ensure_built(&Builds::specs(&["jvm"]), &b.local()).unwrap();
        let recipe = b.state()["builds"][1]["recipe"].clone();
        assert_eq!(recipe["from"], "localhost/bottle/base:latest");
        assert_eq!(recipe["labels"][FEATURES_LABEL], "jvm tools");
        assert!(recipe.get("features").is_none()); // bottle's, not bottle-cli's
        assert!(recipe.get("entrypoint").is_none()); // what it's built on runs, then
    }

    #[test]
    fn a_change_to_a_feature_or_base_makes_its_images_stale() {
        let b = Builds::new();
        let jvm = features::resolve(&Builds::specs(&["jvm"])).unwrap();
        let before = (images::base_inputs().unwrap(), inputs_of(&jvm).unwrap());
        fs::write(
            b.tmp.path().join("features/jvm/install.sh"),
            "#!/bin/sh\necho changed\n",
        )
        .unwrap();
        let after = (images::base_inputs().unwrap(), inputs_of(&jvm).unwrap());
        assert!(before.0 == after.0 && before.1 != after.1);
        fs::write(
            b.tmp.path().join("features/base/install.sh"),
            "#!/bin/sh\necho changed\n",
        )
        .unwrap();
        assert!(images::base_inputs().unwrap() != after.0 && inputs_of(&jvm).unwrap() != after.1);
    }

    #[test]
    fn base_isnt_a_feature_to_ask_for() {
        let b = Builds::new();
        assert!(!features::available().contains(&"base".to_string()));
        assert!(
            features::resolve(&Builds::specs(&["base"]))
                .unwrap_err()
                .contains("base is in every bottle")
        );
        b.feature("needy", json!({"dependsOn": {"base": {}}}));
        assert!(
            features::load("needy")
                .unwrap_err()
                .contains("installed on base already")
        );
    }

    #[test]
    fn stale_images_are_forgotten_and_bottle_cli_deletes_what_nothing_uses() {
        let b = Builds::new();
        let tools = features::resolve(&Builds::specs(&["tools"])).unwrap();
        let current = inputs_of(&tools).unwrap();
        b.image(
            "localhost/bottle/base:with-tools",
            json!({"bottle.features": "tools", "bottle.inputs": current}),
        );
        b.image(
            "localhost/bottle/base:with-old",
            json!({"bottle.features": "tools", "bottle.inputs": "outdated"}),
        );
        b.image(
            "localhost/bottle/base:with-gone",
            json!({"bottle.features": "no-such-feature", "bottle.inputs": "x"}),
        );
        b.image(
            "localhost/bottle/base:with-unlabeled",
            json!({"bottle.features": "tools"}),
        );
        // Someone else's: nothing says what it's made from, and folio can't build it.
        b.image("elsewhere/image:latest", json!({}));
        let removed = remove_stale(&b.local()).unwrap();
        assert_eq!(
            removed,
            [
                "localhost/bottle/base:with-gone",
                "localhost/bottle/base:with-old",
                "localhost/bottle/base:with-unlabeled"
            ]
        );
        let left: Vec<String> = b.state()["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["tag"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            left,
            ["localhost/bottle/base:with-tools", "elsewhere/image:latest"]
        );
        let calls = b.state()["calls"].clone();
        assert_eq!(calls.as_array().unwrap().last().unwrap(), &json!(["gc"]));
    }

    #[test]
    fn a_build_on_an_image_not_built_or_one_that_fails_is_an_error() {
        let b = Builds::new();
        let recipe = json!({"from": "localhost/bottle/base:latest", "steps": []});
        let refused = build_recipe(b.tmp.path(), "t", "inputs", &recipe, &Cli::Local).unwrap_err();
        assert_eq!(refused, "localhost/bottle/base:latest isn't built");
        tempdir::set(&b.env, "FAKE_CLI_FAIL", "build");
        let recipe = json!({"pull": "debian:13", "steps": [{"run": ["true"]}]});
        let failed = build_recipe(
            b.tmp.path(),
            "localhost/bottle/base:latest",
            "aaaa",
            &recipe,
            &Cli::Local,
        )
        .unwrap_err();
        assert_eq!(failed, "bottle-cli image build failed");
        assert_eq!(
            built_labels("localhost/bottle/base:latest", &Cli::Local).unwrap(),
            None
        );
    }

    #[test]
    fn a_build_is_kept_under_its_tag_and_forgetting_it_untags_it() {
        let b = Builds::new();
        let recipe = json!({"pull": "debian:13", "steps": [{"run": ["true"]}], "cmd": ["sshd"], "labels": {"title": "t"}});
        let disk = build_recipe(
            b.tmp.path(),
            "localhost/bottle/base:latest",
            "aaaa",
            &recipe,
            &Cli::Local,
        )
        .unwrap();
        assert_eq!(disk, PathBuf::from("/stages/stage-1.ext4"));
        let labels = built_labels("localhost/bottle/base:latest", &Cli::Local)
            .unwrap()
            .unwrap();
        assert_eq!(
            labels,
            BTreeMap::from([
                ("bottle.inputs".to_string(), "aaaa".to_string()),
                ("title".into(), "t".into())
            ])
        );
        let made = b.state()["builds"][0]["recipe"].clone();
        assert_eq!(
            (&made["steps"], &made["cmd"]),
            (&json!([{"run": ["true"]}]), &json!(["sshd"]))
        );
        assert!(is_current("localhost/bottle/base:latest", "aaaa", &Cli::Local).unwrap());
        assert!(!is_current("localhost/bottle/base:latest", "bbbb", &Cli::Local).unwrap());
        forget("localhost/bottle/base:latest", &Cli::Local).unwrap();
        forget("localhost/bottle/base:latest", &Cli::Local).unwrap(); // gone already: nothing to do
        assert!(built(&Cli::Local).unwrap().is_empty());
        assert!(!is_current("localhost/bottle/base:latest", "aaaa", &Cli::Local).unwrap());
    }
}
