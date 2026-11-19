//! A bottle's agent at work: when it last said so, as an event of its own (Claude Code's hooks run
//! `bottle-event activity` on every prompt, tool use and stop, which POSTs to folio serve's
//! `http://bottle.internal/activity`). The events aren't read for what they say, which the agent
//! could make anything: any is the agent at work, and a while without one is the agent at rest.
//!
//! The last one is a file's time, `activity/<bottle>` in folio's home: folio serve's to write, anyone's
//! to read, kept across folio serve's restarts.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

fn path(bottle: &str) -> PathBuf {
    crate::home().join("activity").join(bottle)
}

/// When `bottle`'s agent last said it was at work; None if it never has.
pub fn last(bottle: &str) -> Option<SystemTime> {
    fs::metadata(path(bottle)).and_then(|m| m.modified()).ok()
}

/// How long an agent without an event is at rest, how long it has to answer, and how long what
/// follows a snapshot is still its: 15 minutes, longer than a foreground command runs (10), or
/// $FOLIO_IDLE_SECONDS (tests).
pub fn idle() -> Duration {
    Duration::from_secs(
        std::env::var("FOLIO_IDLE_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15 * 60),
    )
}

/// Whether `bottle`'s agent is at rest, and has done nothing since a snapshot `taken` then but in
/// the IDLE after it (the rest of the turn that answered its ask, as serve/idle.rs has it); not if
/// it never said it was at work, there being no telling.
pub fn quiet_since(bottle: &str, taken: SystemTime) -> bool {
    last(bottle).is_some_and(|last| {
        SystemTime::now().duration_since(last).unwrap_or_default() >= idle()
            && last <= taken + idle()
    })
}

/// `bottle`'s agent is at work, now.
pub fn record(bottle: &str) -> io::Result<()> {
    let path = path(bottle);
    fs::create_dir_all(path.parent().unwrap())?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    file.set_modified(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_event_is_when_it_came() {
        let _home = crate::tempdir::isolated();
        assert_eq!(last("b"), None);
        let before = SystemTime::now();
        record("b").unwrap();
        let first = last("b").unwrap();
        assert!(first >= before);
        std::thread::sleep(std::time::Duration::from_millis(20));
        record("b").unwrap();
        assert!(last("b").unwrap() > first);
        assert_eq!(last("other"), None);
    }
}
