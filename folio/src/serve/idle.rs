//! Idle snapshots: folio serve's own, of a thread whose agent has been at rest a while, so a thread
//! that's never paused (its runner turned off, say) has a recent snapshot to resume from.
//!
//! An agent at rest is one with no activity event (activity.rs) for IDLE: then its agent is asked
//! to get ready, and the snapshot is taken once it says it is (threads::snapshot_idle), the bottle
//! kept. What's asked while one is under way doesn't count, nor what comes in the IDLE after one's
//! taken: the agent's answer, and the rest of its turn, which may hold a foreground command too. After a snapshot, another needs activity
//! since; one put off (the agent didn't answer, or answered anything else) is asked again IDLE
//! later. An agent that never says it's at work (a bottle without the hooks) is never asked.
//!
//! A thread is found by its bottle among every repo folio knows, and the broker's: a bottle that's
//! no thread's (a sidecar) has no snapshot of its own. What's kept of each is in this process
//! alone; a restart starts afresh, and a snapshot taken since the agent's last event (thread's
//! snapshot_taken) needs no other.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use super::Bound;
use crate::log::log;
use crate::repos::Repo;
use crate::threads::{self, Thread};

/// How long an agent without an event is at rest, how long it has to answer, and how long what
/// follows a snapshot is still its: 15 minutes, longer than a foreground command runs (10), or
/// $FOLIO_IDLE_SECONDS (tests).
fn idle() -> Duration {
    Duration::from_secs(
        std::env::var("FOLIO_IDLE_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15 * 60),
    )
}

/// How often each bottle is looked at: a minute at most.
fn tick() -> Duration {
    (idle() / 4).min(Duration::from_secs(60))
}

/// What's kept of a bottle between looks.
#[derive(Default)]
struct Seen {
    /// A snapshot under way.
    busy: bool,
    /// Events before this don't count: they were a snapshot's.
    after: Option<SystemTime>,
    /// Not asked again before this: it put the last off.
    not_before: Option<SystemTime>,
}

/// Looks at every bottle bound, every tick, for as long as folio serve runs.
pub fn watch(bound: Arc<dyn Bound + Send + Sync>) {
    let seen: Arc<Mutex<HashMap<String, Seen>>> = Arc::default();
    thread::spawn(move || {
        loop {
            thread::sleep(tick());
            for bottle in bound.all() {
                look(&bottle.name, &seen);
            }
        }
    });
}

/// The thread whose bottle is `bottle`, and its repo.
fn thread_of(bottle: &str) -> Option<(Repo, Thread)> {
    crate::repos::known()
        .into_iter()
        .chain([crate::broker::repo()])
        .find_map(|repo| {
            let thread = threads::all(&repo)
                .ok()?
                .into_iter()
                .find(|t| t.bottle == bottle)?;
            Some((repo, thread))
        })
}

fn look(bottle: &str, seen: &Arc<Mutex<HashMap<String, Seen>>>) {
    let Some(last) = crate::activity::last(bottle) else {
        return;
    };
    let now = SystemTime::now();
    {
        let all = seen.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = all.get(bottle)
            && (s.busy
                || s.after.is_some_and(|after| last <= after)
                || s.not_before.is_some_and(|not_before| now < not_before))
        {
            return;
        }
    }
    if now.duration_since(last).unwrap_or_default() < idle() {
        return;
    }
    let Some((repo, thread)) = thread_of(bottle) else {
        return;
    };
    let taken = threads::snapshot_taken(&repo, &thread)
        .map(|t| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(t));
    if taken.is_some_and(|taken| taken >= last) {
        return;
    }
    seen.lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(bottle.to_string())
        .or_default()
        .busy = true;
    let (bottle, seen) = (bottle.to_string(), seen.clone());
    thread::spawn(move || {
        log!("bottle.daemon", "{bottle}: at rest, asking for a snapshot");
        // An engine of its own: one keeps what it learns in cells, for one thread.
        let engine = crate::engine::Engine::default();
        let taken = threads::snapshot_idle(&engine, &repo, &thread, idle());
        let mut all = seen.lock().unwrap_or_else(|e| e.into_inner());
        let s = all.entry(bottle.clone()).or_default();
        s.busy = false;
        let now = SystemTime::now();
        match taken {
            Ok(true) => s.after = Some(now + idle()),
            Ok(false) => s.not_before = Some(now + idle()),
            Err(e) => {
                log!("bottle.daemon", "{bottle}: its snapshot failed: {e}");
                s.not_before = Some(now + idle());
            }
        }
    });
}
