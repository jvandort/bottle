//! Asking a bottle's agent to get ready for a snapshot, and its answer: where folio, which asks, and
//! folio serve, which the answer comes to, meet.
//!
//! The agent is asked by an event from the `snapshot` address (ADDRESS, an address of folio's
//! own, not a bottle's: messages/ issues and checks it as it does a bottle's), and answers by
//! sending to it. A bottle has one ask at most, a file in folio's home, `asks/<bottle>`, saying who
//! asked (its process, and which of its asks): a new one takes the old one's place, and the old
//! answer with it. Whoever asked before goes on waiting, and the next answer is its too, whichever
//! event it answers: there's one thing to be ready for. An answer is `asks/<bottle>.answer`, a count
//! of the answers to the ask and the latest's text, renamed into place; an asker takes each answer
//! once, by its count. An answer is only written while it's asked for, by a process that's still
//! there: one that comes with none is refused, so a bottle can't make a snapshot happen, or an
//! answer wait for a later ask, and an asker gone without a word (killed) leaves only its file,
//! which the next ask replaces.

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The address the agent is asked from and answers.
pub const ADDRESS: &str = "snapshot";

fn ask_path(bottle: &str) -> PathBuf {
    crate::home().join("asks").join(bottle)
}

fn answer_path(bottle: &str) -> PathBuf {
    crate::home().join("asks").join(format!("{bottle}.answer"))
}

/// The answers to the bottle's ask so far, and the latest's text.
fn answers(bottle: &str) -> (u64, String) {
    let Ok(file) = fs::read_to_string(answer_path(bottle)) else {
        return (0, String::new());
    };
    let (count, text) = file.split_once('\n').unwrap_or((&file, ""));
    (count.parse().unwrap_or(0), text.to_string())
}

/// An ask, there until it's dropped (unless a newer one has taken its place).
pub struct Ask {
    bottle: String,
    id: String,
    /// The answers it's taken.
    taken: Cell<u64>,
}

impl Ask {
    /// An ask of `bottle`'s agent, in place of any before it.
    pub fn new(bottle: &str) -> io::Result<Ask> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = ask_path(bottle);
        fs::create_dir_all(path.parent().unwrap())?;
        let id = format!(
            "{} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let _ = fs::remove_file(answer_path(bottle));
        let partial = path.with_extension(format!("{}.partial", std::process::id()));
        fs::write(&partial, &id)?;
        fs::rename(&partial, &path)?;
        Ok(Ask {
            bottle: bottle.into(),
            id,
            taken: Cell::new(0),
        })
    }

    /// The agent's answer, if one has come since it was last taken.
    pub fn take(&self) -> Option<String> {
        let (count, text) = answers(&self.bottle);
        // Fewer than it's taken: a newer ask's, begun afresh.
        if count < self.taken.get() {
            self.taken.set(0);
        }
        if count == self.taken.get() {
            return None;
        }
        self.taken.set(count);
        Some(text)
    }
}

impl Drop for Ask {
    fn drop(&mut self) {
        // A newer ask's are its own.
        if fs::read_to_string(ask_path(&self.bottle)).is_ok_and(|id| id == self.id) {
            let _ = fs::remove_file(ask_path(&self.bottle));
            let _ = fs::remove_file(answer_path(&self.bottle));
        }
    }
}

/// Whether the process that asked, by the ask's id, is still there.
fn asker_there(id: &str) -> bool {
    id.split(' ')
        .next()
        .and_then(|pid| pid.parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// `bottle`'s agent's answer, `text`, for whoever is asking; false if no one is.
pub fn answer(bottle: &str, text: &str) -> io::Result<bool> {
    match fs::read_to_string(ask_path(bottle)) {
        Ok(id) if asker_there(&id) => {}
        _ => return Ok(false),
    }
    let (count, _) = answers(bottle);
    let path = answer_path(bottle);
    let partial = path.with_extension(format!("answer.{}", std::process::id()));
    fs::write(&partial, format!("{}\n{text}", count + 1))?;
    fs::rename(&partial, &path)?;
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
        assert_eq!(ask.take().as_deref(), Some("snapshot"));
        drop(ask);
        assert!(!answer("b", "snapshot").unwrap());
        assert!(!ask_path("b").exists() && !answer_path("b").exists());
    }

    #[test]
    fn a_new_ask_takes_the_old_ones_place_and_answers_both() {
        let _home = crate::tempdir::isolated();
        let old = Ask::new("b").unwrap();
        assert!(answer("b", "a minute").unwrap());
        assert_eq!(old.take().as_deref(), Some("a minute"));
        // Its answer is gone with it: the new ask hasn't been answered.
        let new = Ask::new("b").unwrap();
        assert_eq!(new.take(), None);
        assert_eq!(old.take(), None);
        assert!(answer("b", "snapshot").unwrap());
        assert_eq!(new.take().as_deref(), Some("snapshot"));
        assert_eq!(old.take().as_deref(), Some("snapshot"));
        // The old one done leaves the new one's; the new one done, nothing's asked.
        drop(old);
        assert!(answer("b", "again").unwrap());
        assert_eq!(new.take().as_deref(), Some("again"));
        drop(new);
        assert!(!answer("b", "snapshot").unwrap());
    }

    #[test]
    fn an_ask_whose_asker_is_gone_is_no_ask() {
        let _home = crate::tempdir::isolated();
        fs::create_dir_all(ask_path("b").parent().unwrap()).unwrap();
        // No process has this id: past pid_max.
        fs::write(ask_path("b"), "2147483646 0").unwrap();
        assert!(!answer("b", "snapshot").unwrap());
        let ask = Ask::new("b").unwrap();
        assert!(answer("b", "snapshot").unwrap());
        assert_eq!(ask.take().as_deref(), Some("snapshot"));
    }
}
