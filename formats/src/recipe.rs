//! What `bottle-cli image build RECIPE` builds: steps run on a stage, making a new one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a build does, on what.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// A registry's image it's built on (`debian:trixie-slim`, `name@sha256:...`), imported first:
    /// instead of `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull: Option<String>,
    /// The image it's built on: a tag in the store, or a stage's name; on nothing if None (and no
    /// `pull`). Once bottle-cli has it, always the stage's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// A directory of the host's, read-only in the build's VM at its target, for its steps: what
    /// they install from. Not in the stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<Mount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// One thing a build does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Step {
    /// A command, as root, in `cwd` (`/` if None), with the image's environment so far and `env`;
    /// it must exit 0.
    Run {
        run: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Variables set in the image, for the steps after and for what runs in it.
    Env { env: BTreeMap<String, String> },
}

/// A directory of the host's, at `target` in a VM. A target under /run is on its tmpfs, and
/// leaves nothing on the stage's disk; anywhere else, the directory made for it would stay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub source: PathBuf,
    pub target: String,
}
