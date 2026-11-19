//! Asking a bottle's agent to get ready for a snapshot, and its answer: where folio, which asks, and
//! folio serve, which the answer comes to, meet.
//!
//! The agent is asked by an event from the `snapshot` address (ADDRESS, an address of folio's
//! own, not a bottle's: messages/ issues and checks it as it does a bottle's), and answers by
//! sending to it. An ask is a file in folio's home, `asks/<bottle>`, there from before the event is
//! sent until the asker is done with it; an answer is `asks/<bottle>.answer`, the text, renamed into
//! place, and taken (read and deleted) by the asker. An answer is only written while it's asked
//! for: one that comes with none is refused, so a bottle can't make a snapshot happen, or an answer
//! wait for a later ask.

use std::fs;
use std::io;
use std::path::PathBuf;

/// The address the agent is asked from and answers.
pub const ADDRESS: &str = "snapshot";

fn dir() -> PathBuf {
    crate::home().join("asks")
}

/// An ask, there until it's dropped.
pub struct Ask {
    path: PathBuf,
    answer: PathBuf,
}

impl Ask {
    /// The ask of `bottle`'s agent, made afresh: an answer to an ask before it is gone.
    pub fn new(bottle: &str) -> io::Result<Ask> {
        fs::create_dir_all(dir())?;
        let ask = Ask {
            path: dir().join(bottle),
            answer: dir().join(format!("{bottle}.answer")),
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
    }
}

/// `bottle`'s agent's answer, `text`, for whoever asked; false if no one is asking.
pub fn answer(bottle: &str, text: &str) -> io::Result<bool> {
    if !dir().join(bottle).exists() {
        return Ok(false);
    }
    let partial = dir().join(format!(".{bottle}.answer.{}", std::process::id()));
    fs::write(&partial, text)?;
    fs::rename(&partial, dir().join(format!("{bottle}.answer")))?;
    Ok(true)
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
        // An answer left from before isn't the new ask's.
        let ask = Ask::new("b").unwrap();
        assert_eq!(ask.take(), None);
    }
}
