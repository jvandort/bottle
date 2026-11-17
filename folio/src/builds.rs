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

use bottle_formats::listing::Image;
use bottle_formats::policy::Policy;
use bottle_formats::recipe::Recipe;

use crate::bottle_cli::{Cli, images_cli};
use crate::features::BASE;
use crate::features::{self, Feature};
use crate::images::{self, REPOSITORY};
use crate::runners::Runner;

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
            let recipe = images::image(&features)?;
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
    let recipe = images::base()?;
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
    recipe: &Recipe,
    cli: &Cli,
) -> Result<PathBuf, String> {
    match (&recipe.pull, &recipe.from) {
        (Some(pull), _) => crate::log::say!("images", "Pulling {pull}..."),
        (None, from) => {
            let from = from.as_deref().unwrap_or_default();
            if built_labels(from, cli)?.is_none() {
                return Err(format!("{from} isn't built"));
            }
        }
    }
    let mut made = recipe.clone();
    made.labels.insert(INPUTS_LABEL.into(), inputs.into());
    // The public network, with none of its features' credentials, nothing routed to folio serve;
    // apt repositories kept.
    let network = Policy {
        cache: images::build_cached()?,
        ..Policy::default()
    };
    // Its progress on stderr; the disk of the stage it made, the last line of its stdout.
    let result = cli.build(&made, &network, context, MOUNT, image)?;
    let stdout = String::from_utf8_lossy(&result.stdout);
    match stdout.split_whitespace().last() {
        Some(disk) if result.status.success() => Ok(PathBuf::from(disk)),
        _ => Err("bottle-cli image build failed".into()),
    }
}

/// bottle-cli's images: each tag, its stage, disks and labels.
fn images_of(cli: &Cli) -> Result<Vec<Image>, String> {
    let listed = images_cli(cli, &["images", "--json"])?;
    serde_json::from_str(&listed).map_err(|e| format!("bottle-cli images: {e}"))
}

/// The labels `image` was last built with, or None if it's not built.
fn built_labels(image: &str, cli: &Cli) -> Result<Option<BTreeMap<String, String>>, String> {
    Ok(images_of(cli)?
        .into_iter()
        .find(|i| i.tag == image && !i.disks.is_empty())
        .map(|i| i.labels))
}

/// Every built tag, with its labels.
fn built(cli: &Cli) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    Ok(images_of(cli)?
        .into_iter()
        .map(|i| (i.tag, i.labels))
        .collect())
}

/// Untag `image`; its stages go once nothing uses them (gc).
fn forget(image: &str, cli: &Cli) -> Result<(), String> {
    if images_of(cli)?.iter().any(|i| i.tag == image) {
        images_cli(cli, &["image", "rm", image])?;
    }
    Ok(())
}
