//! Ctrl-C as a failure, as Java has a thread's interrupt: what folio is doing stops at the next step,
//! failing with "interrupted", and whatever undoes it on failure runs, as it would for any other.
//!
//! The terminal sends SIGINT to folio and to whatever it's running at the time, a bottle-cli or a
//! git: that one dies of it, and its failure is "interrupted" (check). folio itself only records
//! it, so a command doesn't die between steps: each step it starts after that is refused (guard),
//! but for the steps of an undoing (shielded), which run to the end, each in a process group of its
//! own, so a second Ctrl-C reaches none of them. A command interrupted exits 130.

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static SHIELDS: AtomicUsize = AtomicUsize::new(0);

pub const INTERRUPTED_MESSAGE: &str = "interrupted";

extern "C" fn record(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Ctrl-C (and SIGTERM) recorded, from now on, not dying of. A program this process runs gets them
/// as they are: a handler isn't inherited across exec.
pub fn install() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a handler that only stores an atomic.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// Whether this command was interrupted.
pub fn happened() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

fn shielded_now() -> bool {
    SHIELDS.load(Ordering::Relaxed) > 0
}

/// Refused, if it was interrupted (but for an undoing).
pub fn check() -> Result<(), String> {
    if happened() && !shielded_now() {
        return Err(INTERRUPTED_MESSAGE.into());
    }
    Ok(())
}

/// `command`, about to start: refused if it was interrupted; in an undoing, in a process group of
/// its own, out of a Ctrl-C's reach.
pub fn guard(command: &mut Command) -> Result<(), String> {
    if shielded_now() {
        command.process_group(0);
        return Ok(());
    }
    check()
}

/// `undo`, run to the end, interrupted or not: what undoes what failed.
pub fn shielded<T>(undo: impl FnOnce() -> T) -> T {
    SHIELDS.fetch_add(1, Ordering::Relaxed);
    let done = undo();
    SHIELDS.fetch_sub(1, Ordering::Relaxed);
    done
}

#[cfg(test)]
pub fn reset() {
    INTERRUPTED.store(false, Ordering::Relaxed);
}

#[cfg(test)]
pub fn interrupt() {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_a_step_is_refused_but_an_undoings_runs() {
        let _env = crate::tempdir::isolated();
        reset();
        assert!(check().is_ok());
        interrupt();
        assert_eq!(check().unwrap_err(), "interrupted");
        assert_eq!(guard(&mut Command::new("true")).unwrap_err(), "interrupted");
        shielded(|| {
            assert!(check().is_ok());
            let mut command = Command::new("true");
            assert!(guard(&mut command).is_ok());
            assert!(command.status().unwrap().success());
        });
        assert!(check().is_err());
        reset();
    }
}
