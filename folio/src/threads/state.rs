//! Where threads are: each one's bottle's state, and its sidecar's.

use std::collections::BTreeMap;

use crate::bottles::{State, Status};
use crate::engine::Engine;
use crate::repos::Repo;
use crate::snapshots;

use super::{Thread, mtime, snapshot};

/// When the thread was last used, its state row (states) saying: its bottle's last use; a paused
/// thread's, when it was paused; else when it was made.
pub fn last_active(repo: &Repo, thread: &Thread, row: &Row) -> f64 {
    if let Some(when) = row
        .status
        .bottle
        .as_ref()
        .map(|b| b.last_active)
        .filter(|w| *w != 0.0)
    {
        return when;
    }
    match snapshot(repo, thread) {
        Some(slot) => mtime(&slot.join(snapshots::CLAUDE_ARCHIVE)),
        None => thread.created,
    }
}

/// `threads`, most recently active first.
pub fn recent(
    repo: &Repo,
    mut threads: Vec<Thread>,
    states: &BTreeMap<String, Row>,
) -> Vec<Thread> {
    let when = |t: &Thread| last_active(repo, t, &states[&t.name]);
    threads.sort_by(|a, b| when(b).total_cmp(&when(a)));
    threads
}

/// A thread's state: its bottle's (Paused, for one with a snapshot and no bottle), and its
/// sidecar's, if it has one.
#[derive(Clone, Debug)]
pub struct Row {
    pub status: Status,
    pub sidecar: Option<State>,
}

/// Each thread's state, by thread name.
pub fn states(
    engine: &Engine,
    repo: &Repo,
    threads: &[Thread],
) -> Result<BTreeMap<String, Row>, String> {
    let mut names: Vec<String> = threads.iter().map(|t| t.bottle.clone()).collect();
    names.extend(
        threads
            .iter()
            .filter(|t| t.sidecar.is_some())
            .map(Thread::sidecar_bottle),
    );
    let rows = engine.status(&names)?;
    let mut result = BTreeMap::new();
    for t in threads {
        let mut status = rows.get(&t.bottle).cloned().unwrap_or_else(|| Status {
            name: t.bottle.clone(),
            state: State::Missing,
            bottle: None,
        });
        if status.state == State::Missing && snapshot(repo, t).is_some() {
            status.state = State::Paused;
        }
        let sidecar = t.sidecar.is_some().then(|| {
            rows.get(&t.sidecar_bottle())
                .map_or(State::Missing, |r| r.state)
        });
        result.insert(t.name.clone(), Row { status, sidecar });
    }
    Ok(result)
}

pub(super) fn state_of(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<State, String> {
    Ok(
        states(engine, repo, std::slice::from_ref(thread))?[&thread.name]
            .status
            .state,
    )
}
