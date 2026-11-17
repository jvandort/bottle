//! folio serve's control protocol: what folio (daemon.rs) asks of folio serve (serve/mod.rs) on its
//! control socket, a JSON line each way, and what it's told of a bottle to answer. Types only:
//! both ends, and what builds a bind (bottles.rs), depend on this, and it on nothing of theirs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A request on folio serve's control socket, a JSON line: {"op": ..., ...}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// What code it runs: Reply's code.
    Ping,
    /// Answer a bottle's machine.
    Bind(Box<Bind>),
    /// Stop answering a bottle's machine: with none left, folio serve stops.
    Release {
        bottle: String,
    },
    Shutdown,
}

/// What folio serve answers a request: whether it did, why not, and to a ping, the code it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Reply {
    pub fn ok() -> Reply {
        Reply {
            ok: true,
            ..Reply::default()
        }
    }

    pub fn refused(error: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            error: Some(error.into()),
            ..Reply::default()
        }
    }
}

/// What folio serve is told of a bottle: its machine, on its runner, and what that routes to folio
/// serve to answer: its repos, features (whose credentials it attaches), permissions, labels and
/// first repo's name (as the list of bottles has them), the bottles it may ssh to, and, on a runner,
/// what to forward there (THERE=HERE).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bind {
    pub bottle: String,
    pub machine: String,
    pub runner: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub repos: Vec<Served>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub ssh: Vec<Grant>,
    #[serde(default)]
    pub forwards: Vec<String>,
}

/// One repo a bottle is served -- a mount's -- with its remotes (specs), under bottle.host/git/<at>.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Served {
    /// The repo its remotes are served from; None: it's gone.
    pub git_dir: Option<PathBuf>,
    pub remotes: Vec<String>,
    pub at: String,
}

/// A bottle another may ssh to: as `name`, at `address`, which is bottle `bottle` with id `id` and
/// no other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub name: String,
    pub bottle: String,
    pub id: String,
    pub address: String,
}
