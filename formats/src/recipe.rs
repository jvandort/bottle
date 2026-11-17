//! What `bottle-cli image build RECIPE` builds: steps run on a stage, making a new one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a build does, on what.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The stage it's built on, by name in the store; on nothing if None.
    #[serde(default)]
    pub from: Option<String>,
    /// A directory of the host's, read-only in the build's VM at its target, for its steps: what
    /// they install from. Not in the stage.
    #[serde(default)]
    pub mount: Option<Mount>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
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
