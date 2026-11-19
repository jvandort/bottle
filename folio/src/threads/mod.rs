//! Threads: one line of work in one repo, and the bottle it's being worked in.
//!
//! A thread has a kebab-case name, unique within its repo; its bottle is named `<repo
//! key>-<thread>`, unique across repos. Its record is in its slot (see store.rs), and its state is
//! read live from bottle, never trusted from the record:
//!
//! ```text
//! running   the bottle is up: attach to it
//! stopped   the VM stopped, its disk intact (a crash, a restart, `bottle stop`): start it, and the
//!           work is all there
//! paused    no bottle, and a snapshot of it (see snapshots.rs): attaching resumes it in a new
//!           bottle, made from the repo's spec as it is now
//! missing   no bottle, and no snapshot: only what's in the lane is left. A thread that has had a
//!           snapshot is never so; one that hasn't is, when its bottle is deleted from under it.
//! ```
//!
//! A thread keeps its latest snapshot, bottle or not: a resume restores it and leaves it, and only
//! the next snapshot, made whole, takes its place (store.rs's slots). With a bottle, it's what the
//! thread was when it was taken, and the bottle has what's happened since.
//!
//! A thread can have a sidecar (`folio sidecar configure`): a bottle of a spec of its own, on a
//! runner of its own, `<thread's bottle>-sidecar`, with the thread's repos, fetch-only, which the
//! thread's bottle reaches as `ssh sidecar`. It's scratch: made just before the thread's bottle is,
//! started with it, and deleted with it, a pause included. So a sidecar given to a thread with a
//! bottle comes with the next one: pause and resume it.
//!
//! Inside the bottle, Claude and a shell each run in a shpool session of their own, started by one
//! exec and attached to by another, so closing a terminal only detaches it, and whatever the session
//! started keeps running. shpool passes the session's output straight through, so it's the
//! terminal's own scrollback that has it.

use crate::notes;
use crate::repos::Repo;

mod lifecycle;
mod pause;
mod record;
mod sessions;
mod sidecar;
mod state;

pub use lifecycle::{create, delete, rename, set_settings};
pub use pause::{losses, pause};
pub use record::{
    Settings, Thread, all, exists, is_name, load, notes_dir, sidecar_base, snapshot_size,
    snapshot_taken,
};
use record::{directory, save, snapshot};
pub use sessions::{attach, copy, exec};
pub use sidecar::{set_sidecar, sidecar_shell};
pub use state::{last_active, recent, states};

pub const WORKSPACE: &str = "/workspace";
pub const CLAUDE: &str = "claude";
pub const SHELL: &str = "shell";
/// What a session's programs are told their terminal is. Not the one attached, whose TERM the
/// bottle may have no terminfo for, and which a session outlives.
const TERM: &str = "xterm-256color";

/// The paths of the thread's bottle's mounts: its repo's checkout, and its notes; the broker, its
/// notes.
fn mounts(thread: &Thread) -> Vec<&'static str> {
    if thread.broker {
        vec![notes::MOUNT]
    } else {
        vec![WORKSPACE, notes::MOUNT]
    }
}

/// Where the thread's next bottle is made: its own runner, else the repo's; None for bottle's
/// default. The broker's is always here.
pub fn runner_of(repo: &Repo, thread: &Thread) -> Result<Option<String>, String> {
    if thread.broker {
        return Ok(None);
    }
    thread.settings.runner(repo)
}

#[cfg(test)]
mod tests {
    use super::sessions::session_command;
    use super::*;

    #[test]
    fn a_session_runs_as_shpool_takes_it() {
        assert_eq!(session_command(SHELL), "sh -c 'exec bash -l'");
        assert!(session_command(CLAUDE).starts_with("sh -c 'set --; if ls"));
    }
}
