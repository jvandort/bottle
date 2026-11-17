//! What bottle-cli says of its machines, images and machine with `--json`: `ls`, `status`,
//! `images`, `info`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where a machine is: made and never started, running, or stopped since.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Created,
    Running,
    Stopped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Created => "created",
            State::Running => "running",
            State::Stopped => "stopped",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `bottle-cli status NAME --json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub state: State,
}

/// A row of `bottle-cli ls --json`: a machine, and how it was made (None for one whose config
/// can't be read).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    pub state: State,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub made: Option<Made>,
}

/// How a machine was made: its resources, its image (by tag where it has one, else its stage's
/// name), and its labels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Made {
    pub cpus: u8,
    pub mem_mib: u32,
    pub storage_gib: u64,
    pub image: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// A row of `bottle-cli images --json`: a tag, the stage it names, the stage's disks (bottom
/// first; none if the stage is gone) and its labels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub tag: String,
    pub name: String,
    #[serde(default)]
    pub disks: Vec<PathBuf>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// `bottle-cli info --json`: the machine bottle-cli runs on, as its VMs can have it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub memory_mib: u64,
    pub cpus: u64,
    /// Its home, $BOTTLE_HOME.
    pub home: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_whose_config_cant_be_read_is_listed_by_name_and_state() {
        let row: Machine = serde_json::from_str(r#"{"name": "m", "state": "stopped"}"#).unwrap();
        assert_eq!((row.state, &row.made), (State::Stopped, &None));
        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            r#"{"name":"m","state":"stopped"}"#
        );
        let made: Machine = serde_json::from_str(
            r#"{"name": "m", "state": "running", "cpus": 2, "mem_mib": 4096, "storage_gib": 64,
                "image": "debian", "labels": {"folio": "1"}}"#,
        )
        .unwrap();
        assert_eq!(made.made.unwrap().labels["folio"], "1");
    }
}
