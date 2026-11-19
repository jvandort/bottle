//! Asking a bottle's agent to get ready for a snapshot, and its answer: where folio, which asks, and
//! folio serve, which the answer comes to, meet.
//!
//! The agent is asked by an event from the `snapshot` address (ADDRESS, an address of folio's
//! own, not a bottle's: messages/ issues and checks it as it does a bottle's), and answers by
//! sending to it. Whoever asks has an ask of its own, a file in folio's home, `asks/<bottle>/<id>`,
//! there from before the event is sent until the asker is done with it; an answer is written beside
//! every ask of the bottle's there is, as `<id>.answer`, renamed into place, and taken (read and
//! deleted) by its asker. So one answer answers them all, however many have asked, and whichever
//! event it answers. An answer is only written while it's asked for: one that comes with none is
//! refused, so a bottle can't make a snapshot happen, or an answer wait for a later ask.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The address the agent is asked from and answers.
pub const ADDRESS: &str = "snapshot";

const ANSWER: &str = ".answer";

fn dir(bottle: &str) -> PathBuf {
    crate::home().join("asks").join(bottle)
}

/// An ask, there until it's dropped.
pub struct Ask {
    path: PathBuf,
    answer: PathBuf,
}

impl Ask {
    /// An ask of `bottle`'s agent, of this asker's own.
    pub fn new(bottle: &str) -> io::Result<Ask> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = dir(bottle);
        fs::create_dir_all(&dir)?;
        let id = format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let ask = Ask {
            path: dir.join(&id),
            answer: dir.join(format!("{id}{ANSWER}")),
        };
        let _ = fs::remove_file(&ask.answer);
        fs::write(&ask.path, "")?;
        Ok(ask)
    }

    /// The agent's answer, if one has come since it was last taken.
    pub fn take(&self) -> Option<String> {
        let text = fs::read_to_string(&self.answer).ok()?;
        let _ = fs::remove_file(&self.answer);
        Some(text)
    }
}

impl Drop for Ask {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.answer);
        // The bottle's directory, once no one's asking.
        if let Some(dir) = self.path.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
}

/// `bottle`'s agent's answer, `text`, for everyone asking; false if no one is.
pub fn answer(bottle: &str, text: &str) -> io::Result<bool> {
    let dir = dir(bottle);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(false);
    };
    let mut answered = false;
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.starts_with('.') || id.ends_with(ANSWER) {
            continue;
        }
        let partial = dir.join(format!(".{id}{ANSWER}.{}", std::process::id()));
        fs::write(&partial, text)?;
        fs::rename(&partial, dir.join(format!("{id}{ANSWER}")))?;
        answered = true;
    }
    Ok(answered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_taken_only_while_asked() {
        let _home = crate::tempdir::isolated();
        assert!(!answer("b", "snapshot").unwrap());
        let ask = Ask::new("b").unwrap();
        assert_eq!(ask.take(), None);
        assert!(answer("b", "not yet").unwrap());
        assert!(!answer("other", "snapshot").unwrap());
        assert_eq!(ask.take().as_deref(), Some("not yet"));
        assert_eq!(ask.take(), None);
        assert!(answer("b", "snapshot").unwrap());
        drop(ask);
        assert!(!answer("b", "snapshot").unwrap());
        // An answer left from before isn't a new ask's.
        let ask = Ask::new("b").unwrap();
        assert_eq!(ask.take(), None);
    }

    #[test]
    fn one_answer_answers_every_ask() {
        let _home = crate::tempdir::isolated();
        let (first, second) = (Ask::new("b").unwrap(), Ask::new("b").unwrap());
        assert!(answer("b", "snapshot").unwrap());
        assert_eq!(first.take().as_deref(), Some("snapshot"));
        // One asker done doesn't undo the other's.
        drop(first);
        assert_eq!(second.take().as_deref(), Some("snapshot"));
        assert!(answer("b", "again").unwrap());
        assert_eq!(second.take().as_deref(), Some("again"));
        drop(second);
        assert!(!dir("b").exists());
    }
}
